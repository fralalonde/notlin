//! Source-bound Map algebra. Planning and emission share the same operand proof.
//! Kotlin's existing static helper preserves ordering, duplicates and null entries
//! without inventing Java instance operators or evaluating either operand twice.

use super::{FactStatus, KotlinType, SemanticProvider, SemanticSymbol, SymbolId, TypeRef};
use std::collections::BTreeSet;
use std::path::Path;
use tree_sitter::Node;

pub(crate) struct MapMerge<'tree> {
    pub left: Node<'tree>,
    pub right: Node<'tree>,
}

pub(crate) fn establish<'tree, P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    expression: Node<'tree>,
    provider: &P,
) -> Option<MapMerge<'tree>> {
    if provider.source_for_file(file)? != source {
        return None;
    }
    Context {
        source,
        file,
        provider,
    }
    .merge(expression, 0)
}

struct Context<'a, P: SemanticProvider + ?Sized> {
    source: &'a str,
    file: &'a Path,
    provider: &'a P,
}

impl<P: SemanticProvider + ?Sized> Context<'_, P> {
    fn merge<'tree>(&self, expression: Node<'tree>, depth: usize) -> Option<MapMerge<'tree>> {
        if depth > 32 {
            return None;
        }
        let (left, right, selector) = match expression.kind() {
            "binary_expression" => {
                let operator = expression.child_by_field_name("operator")?;
                if text(self.source, operator)? != "+" {
                    return None;
                }
                (
                    expression.child_by_field_name("left")?,
                    expression.child_by_field_name("right")?,
                    operator,
                )
            }
            "call_expression" => {
                let callee = super::call_callee_node(expression)?;
                let name = text(self.source, callee)?;
                let stdlib = name == "plus"
                    || super::kotlin_import_lines(self.source).any(|import| {
                        import.split_once(" as ").is_some_and(|(path, alias)| {
                            path.trim() == "kotlin.collections.plus" && alias.trim() == name
                        })
                    });
                if !stdlib || super::direct_trailing_lambda(expression).is_some() {
                    return None;
                }
                let receiver = super::explicit_receiver_node(expression, callee)?;
                if self
                    .source
                    .get(receiver.end_byte()..callee.start_byte())?
                    .trim()
                    != "."
                {
                    return None;
                }
                let arguments = expression
                    .named_children(&mut expression.walk())
                    .find(|child| child.kind() == "value_arguments")?;
                let arguments = named(arguments)
                    .into_iter()
                    .filter(|child| child.kind() == "value_argument")
                    .collect::<Vec<_>>();
                let [argument] = arguments.as_slice() else {
                    return None;
                };
                let values = named(*argument);
                let [value] = values.as_slice() else {
                    return None;
                };
                if value.kind() == "spread_expression"
                    || argument.child_by_field_name("name").is_some()
                {
                    return None;
                }
                (receiver, *value, callee)
            }
            _ => return None,
        };
        if !self.stdlib_plus_visible(selector)
            || !self.map_expression(left, depth + 1)
            || !self.map_expression(right, depth + 1)
        {
            return None;
        }
        if let Some(site) = self.provider.established_call_site(
            self.file,
            selector.start_byte() as u64,
            blake3::hash(self.source.as_bytes()).as_bytes(),
        ) && (site.callee_name.as_deref() != Some("plus")
            || !site.callee_owner.as_deref().is_some_and(|owner| {
                owner == "kotlin.collections.MapsKt"
                    || owner.starts_with("kotlin.collections.MapsKt__")
            }))
        {
            return None;
        }
        Some(MapMerge { left, right })
    }

    fn stdlib_plus_visible(&self, at: Node<'_>) -> bool {
        let package = super::package_name(self.source);
        let imports = super::kotlin_import_lines(self.source).collect::<Vec<_>>();
        if self.provider.symbols_named("plus").iter().any(|symbol| {
            symbol.id.receiver.is_some()
                && (symbol.id.file == self.file
                    || symbol.id.package == package
                    || imports.iter().any(|import| {
                        import.trim() == format!("{}.*", symbol.id.package)
                            || import.split(" as ").next().unwrap_or(import).trim()
                                == format!("{}.{}", symbol.id.package, symbol.id.name)
                    }))
        }) {
            return false;
        }
        let name = text(self.source, at)
            .filter(|name| *name != "+")
            .unwrap_or("plus");
        if super::is_proven_local_function_shadow(at, name, self.source) {
            return false;
        }
        imports.into_iter().all(|import| {
            let (path, alias) = import.split_once(" as ").unwrap_or((import, ""));
            let path = path.trim();
            if path.ends_with(".*") {
                return matches!(path, "kotlin.collections.*" | "kotlin.*" | "java.util.*")
                    || super::source_wildcard_allows_factory(path, "plus", self.provider);
            }
            let visible = if alias.is_empty() {
                path.rsplit('.').next().unwrap_or("")
            } else {
                alias.trim()
            };
            visible != "plus" || path == "kotlin.collections.plus"
        })
    }

    fn map_expression(&self, expression: Node<'_>, depth: usize) -> bool {
        if depth > 32 {
            return false;
        }
        match expression.kind() {
            "parenthesized_expression" => {
                let children = named(expression);
                matches!(children.as_slice(), [inner] if self.map_expression(*inner, depth + 1))
            }
            "binary_expression" => self.merge(expression, depth + 1).is_some(),
            "call_expression" => {
                if self.merge(expression, depth + 1).is_some() {
                    return true;
                }
                let Some(callee) = super::call_callee_node(expression) else {
                    return false;
                };
                let Some(name) = text(self.source, callee) else {
                    return false;
                };
                if matches!(name, "mapOf" | "mutableMapOf" | "hashMapOf" | "linkedMapOf")
                    && super::known_collection_factory_shape(
                        self.source,
                        expression,
                        callee,
                        name,
                        self.provider,
                    )
                {
                    return true;
                }
                if name == "emptyMap"
                    && super::call_argument_count(expression) == 0
                    && super::explicit_receiver_node(expression, callee).is_none()
                    && !super::is_proven_local_shadow(callee, name, self.source)
                    && !super::is_proven_constructor_property_shadow(callee, name, self.source)
                    && !super::same_package_factory_name_is_shadowed(
                        self.source,
                        expression,
                        name,
                        self.provider,
                    )
                    && super::kotlin_import_lines(self.source).all(|import| {
                        let (path, alias) = import.split_once(" as ").unwrap_or((import, ""));
                        if path.trim().ends_with(".*") {
                            return matches!(path.trim(), "kotlin.collections.*" | "kotlin.*")
                                || super::source_wildcard_allows_factory(
                                    path.trim(),
                                    name,
                                    self.provider,
                                );
                        }
                        let visible = if alias.is_empty() {
                            path.rsplit('.').next().unwrap_or("")
                        } else {
                            alias.trim()
                        };
                        visible != name || path.trim() == "kotlin.collections.emptyMap"
                    })
                {
                    return true;
                }
                let FactStatus::Established(target) = super::resolve_required_call(
                    self.source,
                    self.file,
                    expression,
                    callee,
                    name,
                    self.provider,
                ) else {
                    return false;
                };
                self.provider
                    .symbols_named(&target.name)
                    .into_iter()
                    .find(|symbol| symbol.id == target)
                    .is_some_and(|symbol| self.map_symbol(symbol))
            }
            "identifier" | "simple_identifier" => {
                let Some(name) = text(self.source, expression) else {
                    return false;
                };
                let mut scope = expression.parent();
                while let Some(owner) = scope {
                    if matches!(
                        owner.kind(),
                        "block" | "statements" | "function_body" | "lambda_literal"
                    ) {
                        let local = named(owner)
                            .into_iter()
                            .filter(|child| child.end_byte() <= expression.start_byte())
                            .rfind(|child| {
                                child.kind() == "property_declaration"
                                    && named(*child).into_iter().any(|binding| {
                                        binding.kind() == "variable_declaration"
                                            && super::node_name(binding, self.source).as_deref()
                                                == Some(name)
                                    })
                            });
                        if let Some(local) = local {
                            return self.map_declaration(local, depth + 1);
                        }
                    }
                    if matches!(
                        owner.kind(),
                        "function_declaration" | "secondary_constructor"
                    ) && let Some(ty) =
                        super::lexical_parameter_type(self.source, expression, name)
                    {
                        return self.map_type(self.source, self.file, &ty, &mut BTreeSet::new());
                    }
                    // Do not fall through a lambda/catch/loop binding to a
                    // coincidentally named map property in an outer scope.
                    if matches!(
                        owner.kind(),
                        "lambda_literal" | "catch_block" | "for_statement"
                    ) {
                        return false;
                    }
                    if matches!(owner.kind(), "class_declaration" | "object_declaration")
                        && let Some(id) = super::indexed_type_at(
                            owner,
                            self.source,
                            &super::package_name(self.source),
                            self.provider,
                        )
                    {
                        let mut members = self.provider.companion_properties(&id, name);
                        members.extend(self.declared_properties(&id, name));
                        if !members.is_empty() {
                            return self.unique_map_symbol(members);
                        }
                    }
                    scope = owner.parent();
                }
                let package = super::package_name(self.source);
                let candidates = self
                    .provider
                    .symbols_named(name)
                    .into_iter()
                    .filter(|symbol| {
                        symbol.id.owner_path.is_empty()
                            && (symbol.id.package == package
                                || super::kotlin_import_lines(self.source).any(|import| {
                                    import.trim() == format!("{}.*", symbol.id.package)
                                        || import.trim()
                                            == format!("{}.{}", symbol.id.package, name)
                                }))
                    })
                    .collect();
                self.unique_map_symbol(candidates)
            }
            "navigation_expression" => {
                let parts = named(expression);
                let [base, member] = parts.as_slice() else {
                    return false;
                };
                let (Some(base_name), Some(name)) =
                    (text(self.source, *base), text(self.source, *member))
                else {
                    return false;
                };
                let qualifier = base_name.strip_suffix(".Companion").unwrap_or(base_name);
                if !super::is_proven_local_shadow(*base, qualifier, self.source)
                    && let FactStatus::Established(owner) = self.provider.resolve_type_in_file(
                        self.file,
                        &super::package_name(self.source),
                        qualifier,
                    )
                {
                    let mut members = self.provider.companion_properties(&owner, name);
                    members.extend(self.provider.symbols_named(name).into_iter().filter(
                        |symbol| {
                            let mut path = owner.owner_path.clone();
                            path.push(owner.name.clone());
                            symbol.id.file == owner.file
                                && symbol.id.owner_path == path
                                && self.provider.is_java_static_member(&symbol.id)
                        },
                    ));
                    if !members.is_empty() {
                        return self.unique_map_symbol(members);
                    }
                }
                if let Some(classifier) = super::established_direct_property_receiver_type(
                    self.source,
                    expression,
                    expression,
                    self.file,
                    self.provider,
                ) {
                    return self.map_type(
                        self.source,
                        self.file,
                        &TypeRef::parse(&classifier),
                        &mut BTreeSet::new(),
                    );
                }
                let owner = if base_name == "this" {
                    super::enclosing_dispatch_owner(
                        self.source,
                        expression,
                        self.file,
                        self.provider,
                    )
                } else {
                    super::lexical_parameter_classifier(
                        self.source,
                        expression,
                        base_name,
                        self.file,
                        self.provider,
                    )
                };
                if let Some(owner) = owner {
                    return self.unique_map_symbol(self.declared_properties(&owner, name));
                }
                false
            }
            _ => false,
        }
    }

    fn map_declaration(&self, declaration: Node<'_>, depth: usize) -> bool {
        if let Some(ty) =
            super::declaration_type_node(declaration).and_then(|ty| text(self.source, ty))
        {
            return self.map_type(
                self.source,
                self.file,
                &TypeRef::parse(ty),
                &mut BTreeSet::new(),
            );
        }
        named(declaration)
            .into_iter()
            .rev()
            .find(|child| !matches!(child.kind(), "variable_declaration" | "modifiers"))
            .is_some_and(|initializer| self.map_expression(initializer, depth + 1))
    }

    fn unique_map_symbol(&self, symbols: Vec<&SemanticSymbol>) -> bool {
        let mut symbols = symbols
            .into_iter()
            .filter(|symbol| matches!(symbol.id.kind.as_str(), "property" | "field" | "parameter"))
            .collect::<Vec<_>>();
        symbols.sort_by_key(|symbol| symbol.id.clone());
        symbols.dedup_by(|left, right| left.id == right.id);
        matches!(symbols.as_slice(), [symbol] if self.map_symbol(symbol))
    }

    fn declared_properties(&self, owner: &SymbolId, name: &str) -> Vec<&SemanticSymbol> {
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        self.provider
            .symbols_named(name)
            .into_iter()
            .filter(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.owner_path == path
                    && matches!(symbol.id.kind.as_str(), "property" | "field")
            })
            .collect()
    }

    fn map_symbol(&self, symbol: &SemanticSymbol) -> bool {
        let FactStatus::Established(ty) = &symbol.ty else {
            return false;
        };
        let Some(source) = self.provider.source_for_file(&symbol.id.file) else {
            return false;
        };
        self.map_type(source, &symbol.id.file, ty, &mut BTreeSet::new())
    }

    fn map_type(
        &self,
        source: &str,
        file: &Path,
        ty: &TypeRef,
        aliases: &mut BTreeSet<SymbolId>,
    ) -> bool {
        if ty.nullable {
            return false;
        }
        let KotlinType::Named { name, .. } = &ty.ty else {
            return false;
        };
        let package = super::package_name(source);
        if let Some((id, definition)) =
            super::map_alias::resolve_visible_alias(self.provider, file, &package, name)
        {
            if !aliases.insert(id.clone()) {
                return false;
            }
            let Some(source) = self.provider.source_for_file(&id.file) else {
                return false;
            };
            return self.map_type(source, &id.file, &definition.target, aliases);
        }
        // An alias that cannot be bound (for example through an unproved star
        // import) must not be mistaken for a default-imported Map classifier.
        if self
            .provider
            .symbols_named(name)
            .iter()
            .any(|symbol| symbol.id.kind == "typealias")
        {
            return false;
        }
        match self.provider.resolve_type_in_file(file, &package, name) {
            FactStatus::Established(_) | FactStatus::Ambiguous(_) | FactStatus::Inferred(_) => {
                return false;
            }
            FactStatus::Unknown => {}
        }
        let standard = |path: &str| {
            matches!(
                path,
                "kotlin.collections.Map"
                    | "kotlin.collections.MutableMap"
                    | "java.util.Map"
                    | "java.util.HashMap"
                    | "java.util.LinkedHashMap"
                    | "kotlin.collections.HashMap"
                    | "kotlin.collections.LinkedHashMap"
            )
        };
        if name.contains('.') {
            return standard(name);
        }
        let mut explicit = Vec::new();
        for import in super::kotlin_import_lines(source) {
            let (path, alias) = import.split_once(" as ").unwrap_or((import, ""));
            let path = path.trim();
            let visible = if alias.is_empty() {
                path.rsplit('.').next().unwrap_or("")
            } else {
                alias.trim()
            };
            if visible == name {
                explicit.push(path);
            }
            if path.ends_with(".*")
                && !matches!(path, "kotlin.collections.*" | "kotlin.*" | "java.util.*")
                && !super::source_wildcard_allows_factory(path, name, self.provider)
            {
                return false;
            }
        }
        if !explicit.is_empty() {
            return matches!(explicit.as_slice(), [path] if standard(path));
        }
        matches!(
            name.as_str(),
            "Map" | "MutableMap" | "HashMap" | "LinkedHashMap"
        )
    }
}

fn text<'a>(source: &'a str, node: Node<'_>) -> Option<&'a str> {
    node.utf8_text(source.as_bytes()).ok().map(str::trim)
}

fn named(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk())
        .filter(|child| !child.is_extra())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_custom_maps_extensions_nullable_operands_and_shadowed_bindings() {
        let file = Path::new("Model.kt");
        for source in [
            "package sample\nclass Map<K, V>\nfun merge(left: Map<String, Int>, right: Map<String, Int>): Map<String, Int> = left + right\n",
            "package sample\noperator fun Map<String, Int>.plus(other: Map<String, Int>): Map<String, Int> = this\nfun merge(left: Map<String, Int>, right: Map<String, Int>): Map<String, Int> = left + right\n",
            "package sample\nimport other.plus\nfun merge(left: Map<String, Int>, right: Map<String, Int>): Map<String, Int> = left + right\n",
            "package sample\nimport unknown.library.*\nfun merge(left: Map<String, Int>, right: Map<String, Int>): Map<String, Int> = left + right\n",
            "package sample\nfun merge(left: Map<String, Int>?, right: Map<String, Int>): Map<String, Int> = left + right\n",
            "package sample\ntypealias Entries = Map<String, Int>?\nfun merge(left: Entries, right: Entries): Entries = left + right\n",
            "package sample\nfun merge(left: Map<String, Int>, right: Map<String, Int>): String {\n val left: String = \"left\"\n return left + right\n}\n",
            "package sample\nclass Owner(val left: Map<String, Int>) {\n fun merge(right: Map<String, Int>) = listOf(\"x\").map { left -> left + right }\n}\n",
            "package sample\nclass Pretender { fun emptyMap(): String = \"x\" }\nfun merge(other: Pretender, right: Map<String, Int>): String = other.emptyMap() + right\n",
        ] {
            let provider = super::super::SyntaxSemanticProvider::new([(
                file.to_path_buf(),
                source.to_owned(),
            )]);
            let tree = crate::transpiler::parse_tree(source);
            let mut pending = vec![tree.root_node()];
            let mut count = 0;
            while let Some(node) = pending.pop() {
                if node.kind() == "binary_expression"
                    && node
                        .child_by_field_name("operator")
                        .and_then(|op| text(source, op))
                        == Some("+")
                {
                    assert!(
                        establish(source, file, node, &provider).is_none(),
                        "{source}"
                    );
                    count += 1;
                }
                pending.extend(named(node));
            }
            assert!(
                count > 0,
                "fixture must contain a parsed operator: {source}"
            );
        }
    }

    #[test]
    fn proves_declared_properties_companions_locals_and_chained_merges() {
        let file = Path::new("Model.kt");
        for source in [
            "package sample\ninterface Defaults {\n companion object {\n  val base: Map<String, Int> = mapOf()\n }\n}\nfun merge(): Map<String, Int> = mapOf(\"a\" to 1) + Defaults.base\n",
            "package sample\nclass Holder(private val entries: Map<String, Int>) {\n fun merge(other: Holder): Map<String, Int> = this.entries + other.entries\n}\n",
            "package sample\ninterface Defaults {\n companion object {\n  val base: Map<String, Int> = mapOf()\n  val combined: Map<String, Int> = base + base + mapOf()\n }\n}\n",
            "package sample\nfun merge(left: Map<String, Int>, right: Map<String, Int>): Map<String, Int> {\n val selected: Map<String, Int> = left\n return selected.plus(right)\n}\n",
            "package sample\nimport kotlin.collections.Map as Entries\nimport kotlin.collections.plus as merged\nfun merge(left: Entries<String, Int>, right: Entries<String, Int>): Entries<String, Int> = left.merged(right)\n",
            "package sample\ntypealias Entries = Map<String, Int>\nfun merge(left: Entries, right: Entries): Entries = left + right\n",
        ] {
            let provider = super::super::SyntaxSemanticProvider::new([(
                file.to_path_buf(),
                source.to_owned(),
            )]);
            let tree = crate::transpiler::parse_tree(source);
            let mut pending = vec![tree.root_node()];
            let mut count = 0;
            while let Some(node) = pending.pop() {
                if (node.kind() == "binary_expression"
                    && node
                        .child_by_field_name("operator")
                        .and_then(|op| text(source, op))
                        == Some("+"))
                    || (node.kind() == "call_expression"
                        && super::super::call_callee_node(node)
                            .and_then(|callee| text(source, callee))
                            .is_some_and(|name| matches!(name, "plus" | "merged")))
                {
                    assert!(
                        establish(source, file, node, &provider).is_some(),
                        "operation={:?}; source={source}; symbols={:?}",
                        text(source, node),
                        provider.symbols()
                    );
                    count += 1;
                }
                pending.extend(named(node));
            }
            assert!(
                count > 0,
                "source={source}; ast={}",
                tree.root_node().to_sexp()
            );
        }
    }
}
