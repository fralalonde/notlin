//! Source-backed default bodies, selected in the provider's scope.

use super::default_getters::{
    exact_declaration_node, exact_member_node, expression_body, member_has_annotations,
};
use super::{Declaration, DeclarationKind, MemberKind, SourceIndex, SourceLanguage};
use crate::jvm_primitive::JvmPrimitive;
use crate::semantics::{self, FactStatus, SemanticProvider, SymbolId};
use crate::translation_plan::{PlannedConstantValue, PlannedDefaultGetterBody};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub(crate) struct SourceDefault {
    pub target: SymbolId,
    pub method_name: String,
    pub return_type: String,
    pub body: PlannedDefaultGetterBody,
}

impl SourceIndex {
    /// Copying a default onto an otherwise forwarding interface changes its
    /// declaring owner. A descendant that also inherits an override on another
    /// branch would then inherit unrelated defaults. Keep this interface Kotlin
    /// until its provider supplies a Java default that can simply be inherited.
    pub(crate) fn inherited_default_would_conflict(
        &self,
        owner: &Declaration,
        target: &SymbolId,
        method_name: &str,
        provider: &dyn SemanticProvider,
    ) -> bool {
        if owner.kind != DeclarationKind::Interface {
            return false;
        }
        let owner_id = semantics::workspace_symbol(self, owner);
        let declares = |declaration: &Declaration| {
            declaration.members.iter().any(|member| {
                !member.is_static
                    && member.visibility.as_deref() != Some("private")
                    && (member.kind == MemberKind::Method
                        && member.name == method_name
                        && member.parameter_types.is_empty()
                        || member.kind == MemberKind::Property
                            && super::property_accessor_name(&member.name) == method_name)
            })
        };
        let mut pending = self.direct_subtypes(owner);
        let mut seen = HashSet::new();
        while let Some(child) = pending.pop() {
            if !seen.insert(semantics::workspace_symbol(self, child)) || declares(child) {
                // An explicit override shadows the prospective copy too.
                continue;
            }
            pending.extend(self.direct_subtypes(child));
            let mut ancestors = vec![child];
            let mut visited = HashSet::new();
            while let Some(ancestor) = ancestors.pop() {
                let id = semantics::workspace_symbol(self, ancestor);
                if !visited.insert(id.clone()) {
                    continue;
                }
                if id != owner_id
                    && provider.inherits_type(&id, target)
                    && !provider.inherits_type(&id, &owner_id)
                    && declares(ancestor)
                {
                    return true;
                }
                if let Some(file) = self.declaration_source_file(ancestor) {
                    ancestors.extend(
                        ancestor
                            .supertypes
                            .iter()
                            .filter_map(|spelling| self.resolve_type(file, spelling)),
                    );
                }
            }
        }
        false
    }

    /// Java forbids bypassing an override in another direct parent, even when
    /// Kotlin explicitly selects an ancestor body through a different branch.
    pub(crate) fn can_call_interface_super(
        &self,
        owner: &Declaration,
        qualifier: &Declaration,
        name: &str,
        provider: &dyn SemanticProvider,
    ) -> bool {
        let Some(file) = self.declaration_source_file(owner) else {
            return false;
        };
        let Some((target, _)) = self.effective_interface_value_member(qualifier, file, name) else {
            return false;
        };
        let qualifier_id = semantics::workspace_symbol(self, qualifier);
        let target_id = semantics::workspace_symbol(self, target);
        let mut pending = Vec::new();
        for spelling in &owner.supertypes {
            let Some(parent) = self.resolve_type(file, spelling) else {
                if self.canonical_methodless_external_marker(file, spelling) {
                    continue;
                }
                return false;
            };
            let id = semantics::workspace_symbol(self, parent);
            if id == qualifier_id {
                continue;
            }
            if provider.inherits_type(&id, &qualifier_id) {
                return false;
            }
            pending.push(parent);
        }
        let mut seen = HashSet::new();
        while let Some(parent) = pending.pop() {
            let id = semantics::workspace_symbol(self, parent);
            if !seen.insert(id.clone()) {
                continue;
            }
            if provider.inherits_type(&id, &target_id)
                && parent.members.iter().any(|member| {
                    !member.is_static
                        && member.visibility.as_deref() != Some("private")
                        && (member.kind == MemberKind::Method
                            && member.name == name
                            && member.parameter_types.is_empty()
                            || member.kind == MemberKind::Property
                                && super::property_accessor_name(&member.name) == name)
                })
            {
                return false;
            }
            let Some(context) = self.declaration_source_file(parent) else {
                return false;
            };
            for spelling in &parent.supertypes {
                let Some(next) = self.resolve_type(context, spelling) else {
                    if self.canonical_methodless_external_marker(context, spelling) {
                        continue;
                    }
                    return false;
                };
                pending.push(next);
            }
        }
        true
    }

    /// Materializing a Java default can make the JVM initialize this interface
    /// when an implementing class is created. Keep observable companion state
    /// on its existing initialization path until the emitter models it lazily.
    pub(crate) fn interface_default_initialization_is_safe(&self, owner: &Declaration) -> bool {
        if owner.kind != DeclarationKind::Interface || owner.language != SourceLanguage::Kotlin {
            return true;
        }
        let Some(file) = self.declaration_source_file(owner) else {
            return false;
        };
        let source = file.source_text();
        let Some(tree) = self.source_tree(file) else {
            return false;
        };
        let id = semantics::workspace_symbol(self, owner);
        let Some(decl) = exact_declaration_node(tree.root_node(), source, &file.path, &id) else {
            return false;
        };
        let Some(body) = crate::transpiler::kt::child(decl, "class_body") else {
            return true;
        };
        !body
            .named_children(&mut body.walk())
            .filter(|node| node.kind() == "companion_object")
            .filter_map(|node| crate::transpiler::kt::child(node, "class_body"))
            .any(|body| {
                crate::top_level_property_callsite::has_effectful_top_level_initializer(
                    body, source,
                ) || body
                    .named_children(&mut body.walk())
                    .any(|node| node.kind() == "anonymous_initializer")
            })
    }

    pub(crate) fn source_interface_default(
        &self,
        parent: &Declaration,
        name: &str,
        provider: &dyn SemanticProvider,
    ) -> Option<SourceDefault> {
        if parent.kind != DeclarationKind::Interface {
            return None;
        }
        let file = self.declaration_source_file(parent)?;
        let own = parent
            .members
            .iter()
            .filter(|member| {
                !member.is_static
                    && (member.kind == MemberKind::Method
                        && member.name == name
                        && member.parameter_types.is_empty()
                        || member.kind == MemberKind::Property
                            && super::property_accessor_name(&member.name) == name)
            })
            .collect::<Vec<_>>();
        if own.len() > 1 {
            return None;
        }
        let (target, member) = if let Some(member) = own.first() {
            (parent, *member)
        } else {
            // An opaque branch could override or reabstract the known default.
            self.source_default_hierarchy(parent, None)?;
            self.effective_default_getter(parent, file, name)?
        };
        if !member.has_body || member.is_nullable || member.is_mutable || member.is_jvm_field {
            return None;
        }
        let target_file = self.declaration_source_file(target)?;
        let source = target_file.source_text();
        let owner = semantics::workspace_symbol(self, target);
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        let methods = provider
            .symbols_named(&member.name)
            .into_iter()
            .filter(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.owner_path == path
                    && symbol.id.kind
                        == if member.kind == MemberKind::Property {
                            "property"
                        } else {
                            "function"
                        }
                    && symbol.id.parameters.is_empty()
            })
            .collect::<Vec<_>>();
        let [method] = methods.as_slice() else {
            return None;
        };
        let stable = if member.kind == MemberKind::Property {
            provider.has_plain_instance_property_getter(&method.id)
        } else {
            provider.has_stable_noarg_jvm_signature(&method.id)
        };
        if !stable {
            return None;
        }
        let tree = self.source_tree(target_file)?;
        let declaration =
            exact_declaration_node(tree.root_node(), source, &target_file.path, &owner)?;
        let member_node = exact_member_node(declaration, source, member)?;
        if member_has_annotations(member_node) {
            return None;
        }
        let expression = expression_body(member_node, source)?;
        let return_type = super::default_getters::getter_return_type(self, target, member)?;
        let raw_type = return_type.as_ref();
        if super::declaration_type_parameter_shadows(target, raw_type) {
            return None;
        }
        let expected = super::canonical_getter_type(self, target, raw_type, &HashMap::new())?;
        let super::CanonicalGetterType::Named(identity, arguments) = &expected else {
            return None;
        };
        if !arguments.is_empty() {
            return None;
        }
        let return_type = if let Some(primitive) = identity
            .strip_prefix("kotlin.")
            .and_then(JvmPrimitive::kotlin)
        {
            primitive.java.to_owned()
        } else if identity == "kotlin.String" {
            "java.lang.String".to_owned()
        } else if identity == "kotlin.Any" {
            "java.lang.Object".to_owned()
        } else {
            let id: SymbolId = serde_json::from_str(identity).ok()?;
            id.package
                .split('.')
                .filter(|part| !part.is_empty())
                .chain(id.owner_path.iter().map(String::as_str))
                .chain(std::iter::once(id.name.as_str()))
                .collect::<Vec<_>>()
                .join(".")
        };
        if let Some(body) = super::default_getters::proven_forwarding_expression(
            self,
            target,
            member,
            &expected,
            expression,
            Some(provider),
        ) {
            return Some(SourceDefault {
                target: owner,
                method_name: name.to_owned(),
                return_type,
                body: PlannedDefaultGetterBody::SourceForwarding {
                    accessors: body.steps.into_iter().map(|step| step.accessor).collect(),
                },
            });
        }
        let raw = expression.utf8_text(source.as_bytes()).ok()?.trim();
        let scalar = match identity.as_str() {
            "kotlin.Boolean" if matches!(raw, "true" | "false") => {
                Some(PlannedConstantValue::Boolean(raw == "true"))
            }
            "kotlin.String" if semantics::safe_java_string_literal(raw) => {
                Some(PlannedConstantValue::StringLiteral(raw.to_owned()))
            }
            "kotlin.Byte" | "kotlin.Short" | "kotlin.Int" | "kotlin.Long" => {
                let clean = raw.replace('_', "");
                let suffix = clean.ends_with('L') || clean.ends_with('l');
                let digits = clean.trim_end_matches(['L', 'l']);
                let parsed = digits.parse::<i64>().ok();
                parsed.and_then(|value| {
                    let fits = match identity.as_str() {
                        "kotlin.Byte" => !suffix && i8::try_from(value).is_ok(),
                        "kotlin.Short" => !suffix && i16::try_from(value).is_ok(),
                        "kotlin.Int" => !suffix && i32::try_from(value).is_ok(),
                        _ => true,
                    };
                    fits.then_some(PlannedConstantValue::Integer {
                        value,
                        long: identity == "kotlin.Long",
                    })
                })
            }
            _ => None,
        };
        let value = if let Some(value) = scalar {
            value
        } else {
            let (qualifier, entry_name) = raw.rsplit_once('.')?;
            if !semantics::is_java_identifier(entry_name) {
                return None;
            }
            let root = qualifier.split('.').next()?;
            self.source_default_hierarchy(target, Some(root))?;
            let spelling = semantics::annotation_reference_class(
                source,
                &target_file.path,
                expression,
                qualifier,
                Some(provider),
            )?;
            let FactStatus::Established(enum_id) =
                provider.resolve_type_in_file(&target_file.path, &owner.package, &spelling)
            else {
                return None;
            };
            if enum_id.kind != "enum"
                || !enum_id
                    .package
                    .split('.')
                    .filter(|part| !part.is_empty())
                    .chain(enum_id.owner_path.iter().map(String::as_str))
                    .chain(std::iter::once(enum_id.name.as_str()))
                    .all(semantics::is_java_identifier)
            {
                return None;
            }
            let enum_file = self.source_file(&enum_id.file)?;
            let enum_owner = enum_file
                .declarations
                .iter()
                .find(|d| semantics::workspace_symbol(self, d) == enum_id)?;
            if !self.public_constant_type(enum_owner) {
                return None;
            }
            let FactStatus::Established(entries) = provider.enum_entries(&enum_id) else {
                return None;
            };
            let entries = entries
                .into_iter()
                .filter(|entry| entry.name == entry_name)
                .collect::<Vec<_>>();
            let [entry] = entries.as_slice() else {
                return None;
            };
            let actual = serde_json::to_string(&enum_id).ok()?;
            if actual != *identity
                && !super::proven_reference_subtype(
                    self,
                    enum_file,
                    &super::declaration_key(enum_owner),
                    target_file,
                    raw_type,
                )
            {
                return None;
            }
            PlannedConstantValue::EnumEntry(entry.clone())
        };
        Some(SourceDefault {
            target: owner,
            method_name: name.to_owned(),
            return_type,
            body: PlannedDefaultGetterBody::SourceConstant { value },
        })
    }

    pub(crate) fn inherited_source_defaults(
        &self,
        owner: &Declaration,
        provider: &dyn SemanticProvider,
    ) -> Vec<SourceDefault> {
        let Some(file) = self.declaration_source_file(owner) else {
            return Vec::new();
        };
        let mut names = std::collections::BTreeSet::new();
        let mut pending = owner
            .supertypes
            .iter()
            .filter_map(|ty| self.resolve_type(file, ty))
            .collect::<Vec<_>>();
        let mut seen = HashSet::new();
        while let Some(parent) = pending.pop() {
            if parent.kind != DeclarationKind::Interface
                || !seen.insert(semantics::workspace_symbol(self, parent))
            {
                continue;
            }
            names.extend(
                parent
                    .members
                    .iter()
                    .filter(|member| !member.is_static)
                    .filter_map(|member| match member.kind {
                        MemberKind::Method if member.parameter_types.is_empty() => {
                            Some(member.name.clone())
                        }
                        MemberKind::Property if !member.is_mutable => {
                            Some(super::property_accessor_name(&member.name))
                        }
                        _ => None,
                    }),
            );
            let Some(parent_file) = self.declaration_source_file(parent) else {
                continue;
            };
            pending.extend(
                parent
                    .supertypes
                    .iter()
                    .filter_map(|ty| self.resolve_type(parent_file, ty)),
            );
        }
        names
            .into_iter()
            .filter(|name| {
                !owner.members.iter().any(|member| {
                    !member.is_static
                        && (member.kind == MemberKind::Method
                            && member.name == *name
                            && member.parameter_types.is_empty()
                            || member.kind == MemberKind::Property
                                && super::property_accessor_name(&member.name) == *name)
                })
            })
            .filter_map(|name| {
                self.source_default_hierarchy(owner, None)?;
                let (target, _) = self.effective_default_getter(owner, file, &name)?;
                if target.language != SourceLanguage::Kotlin {
                    return None;
                }
                self.source_interface_default(target, &name, provider)
            })
            .collect()
    }

    fn source_default_hierarchy(&self, owner: &Declaration, shadow: Option<&str>) -> Option<()> {
        let mut pending = vec![owner];
        let mut seen = HashSet::new();
        while let Some(current) = pending.pop() {
            if !seen.insert(semantics::workspace_symbol(self, current)) {
                continue;
            }
            if shadow.is_some_and(|name| {
                current.members.iter().any(|m| {
                    matches!(m.kind, MemberKind::Property | MemberKind::Field) && m.name == name
                })
            }) {
                return None;
            }
            let file = self.declaration_source_file(current)?;
            for spelling in &current.supertypes {
                match self.resolve_type(file, spelling) {
                    Some(parent) if parent.kind == DeclarationKind::Interface => {
                        pending.push(parent)
                    }
                    None if self.canonical_methodless_external_marker(file, spelling) => {}
                    _ => return None,
                }
            }
        }
        Some(())
    }

    fn public_constant_type(&self, owner: &Declaration) -> bool {
        let Some(file) = self.declaration_source_file(owner) else {
            return false;
        };
        let source = file.source_text();
        let mut parser = tree_sitter::Parser::new();
        let language = match owner.language {
            SourceLanguage::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            SourceLanguage::Java => tree_sitter_java::LANGUAGE.into(),
        };
        if parser.set_language(&language).is_err() {
            return false;
        }
        let Some(tree) = parser.parse(source, None) else {
            return false;
        };
        let id = semantics::workspace_symbol(self, owner);
        let Some(mut node) = exact_declaration_node(tree.root_node(), source, &file.path, &id)
        else {
            return false;
        };
        loop {
            if owner.language == SourceLanguage::Java
                && matches!(
                    node.kind(),
                    "class_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                )
                && !crate::transpiler::kt::child(node, "modifiers").is_some_and(|modifiers| {
                    modifiers
                        .utf8_text(source.as_bytes())
                        .unwrap_or("")
                        .split_whitespace()
                        .any(|word| word == "public")
                })
            {
                return false;
            }
            if let Some(modifiers) = crate::transpiler::kt::child(node, "modifiers") {
                let text = modifiers.utf8_text(source.as_bytes()).unwrap_or("");
                if text
                    .split_whitespace()
                    .any(|word| matches!(word, "private" | "protected" | "internal"))
                {
                    return false;
                }
            }
            match node.parent() {
                Some(parent) => node = parent,
                None => break,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantics::SyntaxSemanticProvider;
    use crate::workspace::SourceOverlay;

    fn fixture(source: &str) -> (SourceIndex, SyntaxSemanticProvider) {
        let path = std::env::current_dir().unwrap().join("Defaults.kt");
        assert!(
            !crate::transpiler::parse_tree(source)
                .root_node()
                .has_error(),
            "{}",
            crate::transpiler::parse_tree(source).root_node().to_sexp()
        );
        let index = SourceIndex::default()
            .with_overlays(&[SourceOverlay::Replace {
                path: path.clone(),
                language: SourceLanguage::Kotlin,
                source: source.into(),
            }])
            .unwrap();
        let mut provider = SyntaxSemanticProvider::new([(path, source.into())]);
        provider.enrich_workspace_type_facts(&index);
        (index, provider)
    }

    #[test]
    fn source_default_reads_the_current_overlay_body() {
        let source = "package sample\ninterface Defaults { fun choice(): String = \"before\" }\n";
        let (index, provider) = fixture(source);
        let read = |index: &SourceIndex, provider: &SyntaxSemanticProvider| {
            let owner = index
                .declarations()
                .find(|owner| owner.name == "Defaults")
                .unwrap();
            index
                .source_interface_default(owner, "choice", provider)
                .and_then(|value| value.body.source_expression("this"))
        };
        assert_eq!(read(&index, &provider).as_deref(), Some("\"before\""));
        assert_eq!(read(&index, &provider).as_deref(), Some("\"before\""));
        let path = index.files[0].path.clone();
        for (source, expected) in [
            (source.replace("before", "after"), Some("\"after\"")),
            ("package sample\ninterface Defaults { fun choice(): String { println(\"effect\"); return \"after\" } }\n".into(), None),
        ] {
            let overlay = index.with_overlays(&[SourceOverlay::Replace {
                path: path.clone(), language: SourceLanguage::Kotlin, source: source.clone(),
            }]).unwrap();
            let mut updated = SyntaxSemanticProvider::new([(path.clone(), source)]);
            updated.enrich_workspace_type_facts(&overlay);
            assert_eq!(read(&overlay, &updated).as_deref(), expected);
            assert_eq!(read(&index, &provider).as_deref(), Some("\"before\""));
        }
    }

    #[test]
    fn java_defaults_share_typed_constant_and_virtual_body_proofs() {
        let source = "package sample; public interface Defaults {default int answer(){return 42;} default String label(){return \"ok\";} default Kind choice(){return Kind.ONE;} int number(); default int forward(){return this.number();} String maybe(); default String platform(){return maybe();} default int effect(){System.out.println(1);return 42;}}";
        let directory = std::env::current_dir().unwrap();
        let sources = [
            (directory.join("Defaults.java"), source.to_owned()),
            (
                directory.join("Kind.java"),
                "package sample; public enum Kind {ONE}".to_owned(),
            ),
        ];
        let index = SourceIndex::default()
            .with_overlays(
                &sources
                    .iter()
                    .map(|(path, source)| SourceOverlay::Replace {
                        path: path.clone(),
                        language: SourceLanguage::Java,
                        source: source.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let mut provider = SyntaxSemanticProvider::new(sources);
        provider.enrich_workspace_type_facts(&index);
        let owner = index
            .declarations()
            .find(|owner| owner.name == "Defaults")
            .unwrap();
        for (name, expected) in [
            ("answer", Some("42")),
            ("label", Some("\"ok\"")),
            ("choice", Some("sample.Kind.ONE")),
            ("forward", Some("this.number()")),
            ("platform", None),
            ("effect", None),
        ] {
            assert_eq!(
                index
                    .source_interface_default(owner, name, &provider)
                    .and_then(|value| value.body.source_expression("this"))
                    .as_deref(),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn typed_literals_and_enum_values_share_the_same_proof() {
        let (index, provider) = fixture(
            r#"
            package sample
            enum class Kind { ONE }
            interface Defaults {
                fun answer(): Int { return 42 }
                fun wide(): Long = 4_200L
                fun small(): Byte = -12
                fun flag(): Boolean = true
                val label: String get() = "ok"
                fun kind(): Kind = Kind.ONE
            }
        "#,
        );
        let owner = index.files[0]
            .declarations
            .iter()
            .find(|d| d.name == "Defaults")
            .unwrap();
        for (name, ty, body) in [
            ("answer", "int", "42"),
            ("wide", "long", "4200L"),
            ("small", "byte", "-12"),
            ("flag", "boolean", "true"),
            ("getLabel", "java.lang.String", "\"ok\""),
            ("kind", "sample.Kind", "sample.Kind.ONE"),
        ] {
            let found = index
                .source_interface_default(owner, name, &provider)
                .unwrap_or_else(|| panic!("{name}"));
            assert_eq!(found.return_type, ty, "{name}");
            assert_eq!(
                found.body.source_expression("this").unwrap(),
                body,
                "{name}"
            );
        }
    }

    #[test]
    fn effects_annotations_shadowing_and_reabstraction_are_not_constants() {
        let (index, provider) = fixture(
            r#"
            package sample
            enum class Kind { ONE }
            interface Defaults {
                fun effect(): Int { println("effect"); return 42 }
                fun call(): Int = compute()
                fun compute(): Int
                @JvmName("renamed") fun renamed(): Int = 42
                fun nullable(): Int? = 42
                fun kind(): Kind = Kind.ONE
                val Kind: Kind
            }
            interface AbstractAgain : Defaults {
                override fun kind(): Kind
            }
        "#,
        );
        let file = &index.files[0];
        let owner = file
            .declarations
            .iter()
            .find(|d| d.name == "Defaults")
            .unwrap();
        let call = index
            .source_interface_default(owner, "call", &provider)
            .unwrap();
        assert_eq!(
            call.body.source_expression("this").unwrap(),
            "this.compute()"
        );
        for name in ["effect", "renamed", "nullable", "kind"] {
            assert!(
                index
                    .source_interface_default(owner, name, &provider)
                    .is_none(),
                "{name}"
            );
        }
        let abstract_again = file
            .declarations
            .iter()
            .find(|d| d.name == "AbstractAgain")
            .unwrap();
        assert!(
            index
                .source_interface_default(abstract_again, "kind", &provider)
                .is_none()
        );
    }

    #[test]
    fn inherited_generic_calls_properties_and_required_overloads_share_one_chain() {
        let (index, provider) = fixture(
            r#"
            package sample
            interface View
            class Key(private val stored: String) : View {
                val text: String get() = stored
                fun render(): String = stored
                fun render(value: Int): String = "argument"
            }
            interface Base<T : View> { fun key(): T }
            interface Left<T : View> : Base<T>
            interface Right<T : View> : Base<T>
            interface Defaults : Left<Key>, Right<Key> {
                fun getSelected(): Key = key()
                val text: String get() = key().text
                fun rendered(): String { return key().render() }
                fun self(): Defaults = this
                fun opaqueSelf(): Any = this
                fun view(): View = key()
            }
        "#,
        );
        let owner = index.files[0]
            .declarations
            .iter()
            .find(|d| d.name == "Defaults")
            .unwrap();
        for (name, expected) in [
            ("getSelected", "this.key()"),
            ("getText", "this.key().getText()"),
            ("rendered", "this.key().render()"),
            ("self", "this"),
            ("opaqueSelf", "this"),
            ("view", "this.key()"),
        ] {
            let body = index
                .source_interface_default(owner, name, &provider)
                .unwrap_or_else(|| panic!("{name}"));
            assert_eq!(
                body.body.source_expression("this").unwrap(),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn defaults_varargs_function_properties_and_checked_signatures_need_more_than_a_plain_call() {
        for (declarations, body) in [
            ("fun key(value: Int = 0): Key", "fun render(): Key = key()"),
            (
                "fun key(vararg values: Int): Key",
                "fun render(): Key = key()",
            ),
            ("val key: () -> Key", "fun render(): Key = key()"),
            ("fun key(): Unit", "fun render(): Any = key()"),
            ("fun <T> key(): Key", "fun render(): Key = key<Key>()"),
            (
                "@Throws(java.io.IOException::class) fun key(): Key",
                "fun render(): Key = key()",
            ),
            (
                "suspend fun key(): Key",
                "suspend fun render(): Key = key()",
            ),
            ("fun key(): Key?", "fun render(): Key? = key()"),
        ] {
            let source = format!(
                "package sample\nclass Key\ninterface Defaults {{\n{declarations}\n{body}\n}}\n"
            );
            let (index, provider) = fixture(&source);
            let owner = index.files[0]
                .declarations
                .iter()
                .find(|d| d.name == "Defaults")
                .unwrap();
            assert!(
                index
                    .source_interface_default(owner, "render", &provider)
                    .is_none(),
                "{source}"
            );
        }
    }

    #[test]
    fn record_components_and_ordinary_methods_keep_distinct_jvm_names() {
        let (index, provider) = fixture(
            r#"
            package sample
            class Key
            @JvmRecord data class Envelope(val marker: Key) {
                fun key(): Key = marker
            }
            interface Defaults {
                val envelope: Envelope
                fun component(): Key = envelope.marker
                fun ordinary(): Key = envelope.key()
            }
        "#,
        );
        let owner = index.files[0]
            .declarations
            .iter()
            .find(|d| d.name == "Defaults")
            .unwrap();
        assert!(
            index
                .source_interface_default(owner, "component", &provider)
                .is_none()
        );
        let body = index
            .source_interface_default(owner, "ordinary", &provider)
            .unwrap();
        assert_eq!(
            body.body.source_expression("this").unwrap(),
            "this.getEnvelope().key()"
        );
    }
}
