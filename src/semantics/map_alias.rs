use super::{
    FactStatus, KotlinType, MapAliasTypeArguments, SemanticProvider, SymbolId, TypeAliasDefinition,
    Variance,
};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Clone)]
struct Import {
    path: String,
    visible: String,
}

pub(super) fn resolve<P: SemanticProvider + ?Sized>(
    provider: &P,
    file: &Path,
    package: &str,
    spelling: &str,
) -> FactStatus<MapAliasTypeArguments> {
    let Some(source) = provider.source_for_file(file) else {
        return FactStatus::Unknown;
    };
    let Some((actual_package, _)) = source_scope(source) else {
        return FactStatus::Unknown;
    };
    if actual_package != package {
        return FactStatus::Unknown;
    }
    let mut visited = BTreeSet::new();
    match resolve_alias(provider, file, package, spelling.trim(), &mut visited) {
        Some(arguments) => FactStatus::Established(arguments),
        None => FactStatus::Unknown,
    }
}

fn resolve_alias<P: SemanticProvider + ?Sized>(
    provider: &P,
    file: &Path,
    package: &str,
    spelling: &str,
    visited: &mut BTreeSet<SymbolId>,
) -> Option<MapAliasTypeArguments> {
    let (alias_id, definition) = resolve_visible_alias(provider, file, package, spelling)?;
    resolve_definition(provider, alias_id, definition, visited)
}

fn resolve_definition<P: SemanticProvider + ?Sized>(
    provider: &P,
    alias_id: SymbolId,
    definition: TypeAliasDefinition,
    visited: &mut BTreeSet<SymbolId>,
) -> Option<MapAliasTypeArguments> {
    if !visited.insert(alias_id.clone()) || !definition.parameters.is_empty() {
        return None;
    }
    let source = provider.source_for_file(&alias_id.file)?;
    let (origin_package, _) = source_scope(source)?;
    match &definition.target.ty {
        KotlinType::Named { name, arguments }
            if is_map_classifier(provider, &alias_id.file, &alias_id.package, name) =>
        {
            if arguments.len() != 2 {
                return None;
            }
            let key = scalar_java_name(provider, &alias_id.file, &alias_id.package, &arguments[0])?;
            let value =
                scalar_java_name(provider, &alias_id.file, &alias_id.package, &arguments[1])?;
            Some(MapAliasTypeArguments { key, value })
        }
        KotlinType::Named { name, arguments } if arguments.is_empty() => {
            if let Some((target_package, target_name)) = name.rsplit_once('.') {
                let (target_id, target_definition) =
                    find_alias(provider, target_package, target_name)?;
                resolve_definition(provider, target_id, target_definition, visited)
            } else {
                resolve_alias(provider, &alias_id.file, &origin_package, name, visited)
            }
        }
        _ => None,
    }
}

pub(super) fn resolve_visible_alias<P: SemanticProvider + ?Sized>(
    provider: &P,
    file: &Path,
    package: &str,
    spelling: &str,
) -> Option<(SymbolId, TypeAliasDefinition)> {
    let source = provider.source_for_file(file)?;
    let (_, imports) = source_scope(source)?;
    let explicit = imports
        .iter()
        .filter(|import| import.visible == spelling && !import.path.ends_with(".*"))
        .map(|import| {
            let (target_package, target_name) = import.path.rsplit_once('.')?;
            find_alias(provider, target_package, target_name)
        })
        .collect::<Option<Vec<_>>>()?;
    let local = find_alias(provider, package, spelling)
        .into_iter()
        .collect::<Vec<_>>();
    if explicit.len() + local.len() != 1 {
        return None;
    }

    let selected = explicit.into_iter().chain(local).next()?;
    let local_types = provider
        .symbols_named(spelling)
        .into_iter()
        .filter(|symbol| {
            symbol.id.package == package && is_type_kind(&symbol.id.kind) && symbol.id != selected.0
        })
        .count();
    if local_types != 0 {
        return None;
    }
    // A wildcard can introduce a competing type alias or classifier. Source
    // discovery does not establish that the package is closed-world.
    if imports
        .iter()
        .any(|import| import.path.ends_with(".*") && import.path != "kotlin.*")
    {
        return None;
    }
    Some(selected)
}

fn find_alias<P: SemanticProvider + ?Sized>(
    provider: &P,
    package: &str,
    name: &str,
) -> Option<(SymbolId, TypeAliasDefinition)> {
    let candidates = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| symbol.id.package == package && symbol.id.kind == "typealias")
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return None;
    }
    let id = candidates.into_iter().next()?;
    // This query proves file/package bindings. Nested aliases require a
    // lexical owner path that this interface deliberately does not guess.
    if !id.owner_path.is_empty() {
        return None;
    }
    let definition = provider.type_alias_definition(package, name)?;
    Some((id, definition))
}

fn is_map_classifier<P: SemanticProvider + ?Sized>(
    provider: &P,
    file: &Path,
    package: &str,
    name: &str,
) -> bool {
    let expected = match name {
        "Map" => "Map",
        "MutableMap" => "MutableMap",
        "kotlin.collections.Map" => "Map",
        "kotlin.collections.MutableMap" => "MutableMap",
        _ => return false,
    };
    if name.starts_with("kotlin.collections.") {
        return true;
    }
    let Some(source) = provider.source_for_file(file) else {
        return false;
    };
    if provider
        .symbols_named(expected)
        .iter()
        .any(|symbol| is_type_kind(&symbol.id.kind) && symbol.id.package == package)
    {
        return false;
    }
    let Some((_, imports)) = source_scope(source) else {
        return false;
    };
    if imports.iter().any(|import| {
        import.visible == expected && import.path != format!("kotlin.collections.{expected}")
    }) {
        return false;
    }
    !imports.iter().any(|import| {
        import.path.ends_with(".*")
            && import.path != "kotlin.*"
            && import.path != "kotlin.collections.*"
    })
}

fn scalar_java_name<P: SemanticProvider + ?Sized>(
    provider: &P,
    file: &Path,
    package: &str,
    argument: &super::TypeArgument,
) -> Option<String> {
    if argument.variance != Variance::Invariant {
        return None;
    }
    let (name, nullable) = flatten_nullable(argument.ty.as_ref()?)?;
    let java = match name {
        "String" | "kotlin.String" => "java.lang.String",
        "Boolean" | "kotlin.Boolean" => "java.lang.Boolean",
        "Byte" | "kotlin.Byte" => "java.lang.Byte",
        "Short" | "kotlin.Short" => "java.lang.Short",
        "Int" | "kotlin.Int" => "java.lang.Integer",
        "Long" | "kotlin.Long" => "java.lang.Long",
        "Float" | "kotlin.Float" => "java.lang.Float",
        "Double" | "kotlin.Double" => "java.lang.Double",
        "Char" | "kotlin.Char" => "java.lang.Character",
        "Any" | "kotlin.Any" => "java.lang.Object",
        "Number" | "kotlin.Number" => "java.lang.Number",
        _ => return None,
    };
    if !name.starts_with("kotlin.") {
        let source = provider.source_for_file(file)?;
        let (_, source_imports) = source_scope(source)?;
        if provider
            .symbols_named(name)
            .iter()
            .any(|symbol| is_type_kind(&symbol.id.kind) && symbol.id.package == package)
            || source_imports
                .iter()
                .any(|import| import.visible == name && import.path != format!("kotlin.{name}"))
            || source_imports.iter().any(|import| {
                import.path.ends_with(".*")
                    && import.path != "kotlin.*"
                    && import.path != "kotlin.collections.*"
            })
        {
            return None;
        }
    }
    let _nullable_is_intentionally_erased = nullable;
    Some(java.to_owned())
}

fn flatten_nullable(ty: &KotlinType) -> Option<(&str, bool)> {
    match ty {
        KotlinType::Primitive(name) => Some((name, false)),
        KotlinType::Named { name, arguments } if arguments.is_empty() => Some((name, false)),
        KotlinType::Nullable(inner) => {
            let (name, _) = flatten_nullable(inner)?;
            Some((name, true))
        }
        _ => None,
    }
}

fn is_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "interface" | "object" | "enum" | "annotation" | "typealias"
    )
}

fn source_scope(source: &str) -> Option<(String, Vec<Import>)> {
    let tree = crate::transpiler::parse_tree(source);
    if tree.root_node().has_error() {
        return None;
    }
    let root = tree.root_node();
    let mut package = String::new();
    let mut imports = Vec::new();
    for child in root.named_children(&mut root.walk()) {
        match child.kind() {
            "package_header" => {
                let qualified = child
                    .named_children(&mut child.walk())
                    .find(|node| node.kind() == "qualified_identifier")?;
                package = qualified
                    .utf8_text(source.as_bytes())
                    .ok()?
                    .replace(' ', "");
            }
            "import" => {
                let declaration = child
                    .utf8_text(source.as_bytes())
                    .ok()?
                    .trim()
                    .strip_prefix("import ")?;
                let (path, alias) = declaration
                    .split_once(" as ")
                    .map_or((declaration, None), |(path, alias)| {
                        (path, Some(alias.trim()))
                    });
                let path = path.trim().to_owned();
                let visible = alias
                    .map(str::to_owned)
                    .or_else(|| path.rsplit('.').next().map(str::to_owned))?;
                imports.push(Import { path, visible });
            }
            _ => {}
        }
    }
    Some((package, imports))
}
