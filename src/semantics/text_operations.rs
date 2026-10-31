//! One receiver/callee contract for Kotlin's zero-argument whitespace helpers.
//! Java's trim/isBlank use different whitespace sets. Forward through Kotlin's
//! stdlib, preserving String versus CharSequence results and nullable queries.

use super::{FactStatus, SemanticProvider, standard_external_type, stdlib_import};
use crate::facts::protocol::CallSite;
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Trim,
    TrimStart,
    TrimEnd,
    IsBlank,
    IsNotBlank,
    IsNullOrBlank,
    IsNullOrEmpty,
    OrEmpty,
}

impl Operation {
    fn named(name: &str) -> Option<Self> {
        Some(match name.trim_matches('`') {
            "trim" => Self::Trim,
            "trimStart" => Self::TrimStart,
            "trimEnd" => Self::TrimEnd,
            "isBlank" => Self::IsBlank,
            "isNotBlank" => Self::IsNotBlank,
            "isNullOrBlank" => Self::IsNullOrBlank,
            "isNullOrEmpty" => Self::IsNullOrEmpty,
            "orEmpty" => Self::OrEmpty,
            _ => return None,
        })
    }
    fn accepts_null(self) -> bool {
        matches!(
            self,
            Self::IsNullOrBlank | Self::IsNullOrEmpty | Self::OrEmpty
        )
    }
    fn boolean(self) -> bool {
        matches!(
            self,
            Self::IsBlank | Self::IsNotBlank | Self::IsNullOrBlank | Self::IsNullOrEmpty
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Receiver {
    String,
    CharSequence,
}

pub(crate) struct TextCall<'tree> {
    pub receiver: Node<'tree>,
    pub operation: Operation,
    pub receiver_kind: Receiver,
}

impl TextCall<'_> {
    pub fn return_type(&self) -> super::TypeRef {
        let mut result = super::TypeRef::parse(if self.operation.boolean() {
            "Boolean"
        } else {
            self.result_classifier()
        });
        result.nullable = self.receiver.parent().is_some_and(|navigation| {
            navigation
                .children(&mut navigation.walk())
                .any(|child| child.kind() == "?.")
        });
        result
    }
    pub fn result_classifier(&self) -> &'static str {
        if self.operation.boolean() {
            "kotlin.Boolean"
        } else if self.receiver_kind == Receiver::String {
            "java.lang.String"
        } else {
            "java.lang.CharSequence"
        }
    }

    pub fn java(&self, receiver: &str) -> String {
        use Operation::*;
        let ty = if self.receiver_kind == Receiver::String {
            "java.lang.String"
        } else {
            "java.lang.CharSequence"
        };
        let value = format!("(({ty}) ({receiver}))");
        let nonnull = if self.operation.accepts_null() {
            format!("java.util.Objects.requireNonNullElse({value}, \"\")")
        } else {
            value
        };
        match self.operation {
            Trim | TrimStart | TrimEnd => {
                let method = match self.operation {
                    Trim => "trim",
                    TrimStart => "trimStart",
                    _ => "trimEnd",
                };
                let call =
                    format!("kotlin.text.StringsKt.{method}((java.lang.CharSequence) {nonnull})");
                if self.receiver_kind == Receiver::String {
                    format!("{call}.toString()")
                } else {
                    call
                }
            }
            IsBlank | IsNullOrBlank => format!("kotlin.text.StringsKt.isBlank({nonnull})"),
            IsNotBlank => format!("(!kotlin.text.StringsKt.isBlank({nonnull}))"),
            IsNullOrEmpty => format!("({nonnull}.length() == 0)"),
            OrEmpty => nonnull,
        }
    }
}

fn source_operation(source: &str, call: Node<'_>, spelling: &str) -> Option<Operation> {
    match stdlib_import::binding(source, call, spelling, "kotlin.text") {
        stdlib_import::Binding::Absent => Operation::named(spelling),
        stdlib_import::Binding::Standard(name) => Operation::named(&name),
        stdlib_import::Binding::Foreign => None,
    }
}

/// A function import whose uses are lowered through a qualified stdlib call
/// must not become a Java type import. This does not admit any call by name.
pub(crate) fn is_lowered_import(source: &str, import: Node<'_>) -> bool {
    super::stdlib_import::path(source, import)
        .and_then(|path| {
            path.strip_prefix("kotlin.text.").map(|name| {
                Operation::named(name).is_some() || matches!(name, "lowercase" | "uppercase")
            })
        })
        .unwrap_or(false)
}

pub(crate) fn establish<'tree, P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'tree>,
    provider: &P,
) -> Option<TextCall<'tree>> {
    if call.kind() != "call_expression"
        || call.has_error()
        || provider.source_for_file(file)? != source
        || super::call_argument_count(call) != 0
        || super::direct_trailing_lambda(call).is_some()
        || call
            .named_children(&mut call.walk())
            .any(|node| node.kind() == "type_arguments")
    {
        return None;
    }
    let callee = super::call_callee_node(call)?;
    let spelling = callee.utf8_text(source.as_bytes()).ok()?.trim_matches('`');
    let receiver = super::explicit_receiver_node(call, callee)?;
    let requested = source_operation(source, call, spelling)?;
    if provider.symbols_named(spelling).iter().any(|symbol| {
        symbol.id.receiver.is_some() && matches!(symbol.id.kind.as_str(), "function" | "method")
    }) {
        return None;
    }
    let site = super::string_case::call_site(source, file, call, provider);
    // An import alias cannot replace an applicable Any member. An authoritative
    // site can settle more overloads; the source-only rule must keep member
    // precedence rather than treating an alias as a dispatch result.
    if site.is_none() && matches!(spelling, "hashCode" | "toString") {
        return None;
    }
    let operation = if let Some(site) = &site {
        if site.callee_owner.as_deref() != Some("kotlin.text.StringsKt")
            || !site.parameter_types.is_empty()
            || site.result_nullable
        {
            return None;
        }
        let operation = Operation::named(site.callee_name.as_deref()?)?;
        if operation != requested {
            return None;
        }
        operation
    } else {
        if !matches!(
            super::resolve_required_call(source, file, call, callee, spelling, provider),
            FactStatus::Unknown
        ) {
            return None;
        }
        requested
    };
    let classifier =
        super::explicit_receiver_type(source, call, callee, file, provider).or_else(|| {
            operation
                .accepts_null()
                .then(|| super::guarded_receiver_classifier(source, file, receiver, provider))
                .flatten()
        })?;
    let receiver_kind = match standard_external_type(source, provider, &classifier, false)?.as_str()
    {
        "java.lang.String" => Receiver::String,
        "java.lang.CharSequence" if operation != Operation::OrEmpty => Receiver::CharSequence,
        _ => return None,
    };
    let fact = TextCall {
        receiver,
        operation,
        receiver_kind,
    };
    if site.as_ref().is_some_and(|site| !valid_result(site, &fact)) {
        return None;
    }
    Some(fact)
}

fn valid_result(site: &CallSite, fact: &TextCall<'_>) -> bool {
    let (result, descriptor) = if fact.operation.boolean() {
        ("kotlin.Boolean", "()Z")
    } else if fact.receiver_kind == Receiver::String {
        ("kotlin.String", "()Ljava/lang/String;")
    } else {
        ("kotlin.CharSequence", "()Ljava/lang/CharSequence;")
    };
    site.result_type.as_deref() == Some(result)
        && site.jvm_descriptor.as_deref() == Some(descriptor)
}

/// A fact for a foreign callee must not fall through to a same-spelled Java
/// member. Source-owned methods on unrelated receiver types keep their path.
pub(super) fn claims<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    callee: Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    if call.kind() != "call_expression" {
        return false;
    }
    let operation = Operation::named(name).or_else(|| source_operation(source, call, name));
    let Some(operation) = operation else {
        return false;
    };
    let Some(receiver) = super::explicit_receiver_node(call, callee) else {
        return false;
    };
    let classifier =
        super::explicit_receiver_type(source, call, callee, file, provider).or_else(|| {
            operation
                .accepts_null()
                .then(|| super::guarded_receiver_classifier(source, file, receiver, provider))
                .flatten()
        });
    classifier
        .and_then(|name| standard_external_type(source, provider, &name, false))
        .is_some_and(|name| matches!(name.as_str(), "java.lang.String" | "java.lang.CharSequence"))
}

#[cfg(test)]
mod tests {
    use super::super::{SemanticSymbol, SymbolId, SyntaxSemanticProvider};
    use super::*;

    fn call(tree: &tree_sitter::Tree) -> Node<'_> {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                return node;
            }
            stack.extend(node.named_children(&mut node.walk()));
        }
        panic!("missing call");
    }

    fn lower(source: &str) -> Option<String> {
        let file = Path::new("Reader.kt");
        let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]);
        let tree = super::super::parse_source(file, source).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{source}\n{}",
            tree.root_node().to_sexp()
        );
        let fact = establish(source, file, call(&tree), &provider)?;
        Some(fact.java(fact.receiver.utf8_text(source.as_bytes()).unwrap()))
    }

    #[test]
    fn nullable_queries_and_qualified_text_types_keep_their_contracts() {
        for source in [
            "class Reader { fun run(value: kotlin.String): kotlin.String = value.trimEnd(); }",
            "class Reader { fun run(value: kotlin.CharSequence): kotlin.CharSequence = value.trimStart(); }",
            "class Reader { fun run(value: String?): Boolean = (value).isNullOrBlank(); }",
            "class Reader(private val value: String?) { fun run(): Boolean = this.value.isNullOrEmpty(); }",
            "class Reader { fun run(value: String?): String? = (value)?.trim(); }",
        ] {
            assert!(lower(source).is_some(), "{source}");
        }
        for source in [
            "class Reader { fun run(value: String?): String = value.trim(); }",
            "class Reader { fun run(value: String): String = value.trim('x'); }",
            "class String\nclass Reader { fun run(value: String): String = value.trim(); }",
            "import foreign.trim\nclass Reader { fun run(value: String): String = value.trim(); }",
            "fun String.trim(): String = \"custom\"\nclass Reader { fun run(value: String): String = value.trim(); }",
        ] {
            assert!(lower(source).is_none(), "{source}");
        }
    }

    #[test]
    fn aliases_follow_the_imported_operation_but_do_not_override_members() {
        let source = "import kotlin.text.trimEnd /* binding */ as `clean`\nclass Reader { fun run(value: String): String = value.`clean`(); }";
        assert!(lower(source).is_some_and(|java| java.contains("StringsKt.trimEnd(")));
        let source = "import kotlin.text.trim as hashCode\nclass Reader { fun run(value: String): Int = value.hashCode(); }";
        assert!(lower(source).is_none());
        let source = "import foreign.trim as clean\nclass Reader { fun run(value: String): String = value.clean(); }";
        assert!(lower(source).is_none());
    }

    #[test]
    fn source_wildcard_type_binding_requires_an_indexed_package_without_the_name() {
        let source = "package sample\nimport domain.*\nclass Reader(val value: String) { fun clean(): Boolean = value.isBlank(); }";
        let file = Path::new("Reader.kt");
        let tree = super::super::parse_source(file, source).unwrap();
        for (declaration, expected) in [
            (None, false),
            (Some("package domain\nclass Other\n"), true),
            (Some("package domain\nclass String\n"), false),
        ] {
            let mut files = vec![(file.to_path_buf(), source.to_owned())];
            if let Some(declaration) = declaration {
                files.push(("Other.kt".into(), declaration.to_owned()));
            }
            let provider = SyntaxSemanticProvider::new(files);
            assert_eq!(
                establish(source, file, call(&tree), &provider).is_some(),
                expected,
                "{declaration:?}"
            );
        }
    }

    struct WithSite {
        inner: SyntaxSemanticProvider,
        site: CallSite,
        hash: [u8; 32],
    }
    impl SemanticProvider for WithSite {
        fn resolve(&self, name: &str) -> FactStatus<SymbolId> {
            self.inner.resolve(name)
        }
        fn symbols(&self) -> &[SemanticSymbol] {
            self.inner.symbols()
        }
        fn source_for_file(&self, file: &Path) -> Option<&str> {
            self.inner.source_for_file(file)
        }
        fn established_call_site(
            &self,
            _: &Path,
            offset: u64,
            hash: &[u8; 32],
        ) -> Option<CallSite> {
            (*hash == self.hash && offset == self.site.byte_offset).then(|| self.site.clone())
        }
    }

    #[test]
    fn authoritative_sites_require_the_same_operation_shape_result_and_snapshot() {
        let source = "class Reader { fun run(value: String): String = value.trim(); }";
        let file = Path::new("Reader.kt");
        let tree = super::super::parse_source(file, source).unwrap();
        let node = call(&tree);
        let valid = CallSite {
            byte_offset: super::super::kotlin_call_site_offset(node).unwrap() as u64,
            text: "trim()".into(),
            status: crate::facts::protocol::SiteStatus::Established,
            callee_owner: Some("kotlin.text.StringsKt".into()),
            callee_name: Some("trim".into()),
            parameter_types: vec![],
            result_type: Some("kotlin.String".into()),
            result_nullable: false,
            jvm_descriptor: Some("()Ljava/lang/String;".into()),
        };
        let provider = |site| WithSite {
            inner: SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]),
            site,
            hash: *blake3::hash(source.as_bytes()).as_bytes(),
        };
        assert!(establish(source, file, node, &provider(valid.clone())).is_some());
        for variant in 0..6 {
            let mut site = valid.clone();
            match variant {
                0 => site.callee_owner = Some("foreign.StringsKt".into()),
                1 => site.callee_name = Some("trimStart".into()),
                2 => site.parameter_types.push("kotlin.CharArray".into()),
                3 => site.result_type = Some("kotlin.Boolean".into()),
                4 => site.result_nullable = true,
                _ => site.jvm_descriptor = Some("()I".into()),
            }
            assert!(
                establish(source, file, node, &provider(site)).is_none(),
                "variant {variant}"
            );
        }
        let changed = source.replace("value.trim()", "(value).trim()");
        let changed_tree = super::super::parse_source(file, &changed).unwrap();
        assert!(establish(&changed, file, call(&changed_tree), &provider(valid)).is_none());
    }

    #[test]
    fn parenthesized_receivers_use_their_inner_type() {
        let source = "class Reader { fun clean(value: String): String = (value).trim(); }";
        let file = Path::new("Reader.kt");
        let provider =
            super::super::SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]);
        let tree = super::super::parse_source(file, source).unwrap();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                let callee = super::super::call_callee_node(node).unwrap();
                assert!(
                    establish(source, file, node, &provider).is_some(),
                    "receiver={:?}; resolution={:?}",
                    super::super::explicit_receiver_type(source, node, callee, file, &provider),
                    super::super::resolve_required_call(
                        source, file, node, callee, "trim", &provider
                    )
                );
                return;
            }
            stack.extend(node.named_children(&mut node.walk()));
        }
        panic!("missing call");
    }
}
