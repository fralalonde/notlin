//! Source-backed contracts for the narrow set of Lombok-generated getters
//! whose Java ABI can be established from accepted output. Callers must prove
//! that Lombok is enabled and the path has no applicable `lombok.config`; a
//! source marker alone does not establish the active Lombok accessor profile.

use crate::java_ir::{JavaElement, JavaSyntaxNode, render};
use crate::planning::AcceptedJavaFile;
use crate::semantics::{
    FactStatus, JvmSignature, KotlinType, ReferenceReturnNullability, SemanticProvider, SymbolId,
    TypeRef,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedGetterContract {
    pub output_path: PathBuf,
    pub output_hash: [u8; 32],
    pub owner: SymbolId,
    pub origin: SymbolId,
    pub target: SymbolId,
    pub property_name: String,
    pub method_name: String,
    pub result_type: TypeRef,
    pub jvm_signature: JvmSignature,
    pub jvm_primitive: bool,
    pub nullability: ReferenceReturnNullability,
    pub dependencies: Vec<SymbolId>,
}

/// Collect only contracts that are directly represented by one accepted,
/// ordinary top-level Java class and one uniquely indexed Kotlin property.
/// Marker annotations are argument-free, so Lombok's default public instance
/// getter name and no-throws signature apply without configuration overrides.
pub(crate) fn collect(
    file: &AcceptedJavaFile,
    provider: &dyn SemanticProvider,
) -> Vec<GeneratedGetterContract> {
    let output = render(&file.unit);
    let output_hash = *blake3::hash(output.as_bytes()).as_bytes();
    if has_static_import(&file.unit.root) {
        return Vec::new();
    }
    let imports = lombok_imports(&file.unit.root);
    let wildcard_imports = wildcard_imports(&file.unit.root);
    let mut contracts = Vec::new();

    for node in file
        .unit
        .root
        .children
        .iter()
        .filter_map(element_node)
        .filter(|node| node.kind == "class_declaration")
    {
        let Some(class_name) = declaration_name(node) else {
            continue;
        };
        let owners = file
            .owners
            .iter()
            .filter(|owner| {
                owner.kind == "class" && owner.name == class_name && owner.owner_path.is_empty()
            })
            .collect::<Vec<_>>();
        let [owner] = owners.as_slice() else {
            continue;
        };
        let Some(marker) = getter_marker(node, &imports) else {
            continue;
        };
        if direct_child_kind(node, "type_parameters").is_some() {
            continue;
        }
        let class_path = vec![class_name.clone()];
        let Some(body) = field(node, "body") else {
            continue;
        };
        let candidates = eligible_fields(body, marker, &imports);
        let properties = provider
            .symbols_in_file(&owner.file)
            .into_iter()
            .filter(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.module == owner.module
                    && symbol.id.package == owner.package
                    && symbol.id.owner_path == class_path
                    && symbol.id.kind == "property"
            })
            .collect::<Vec<_>>();

        for candidate in candidates {
            let field_spelling = candidate.field_spelling;
            let property_name = candidate.property_name;
            let method_name = candidate.method_name;
            let nullability = candidate.nullability;
            let matching = properties
                .iter()
                .filter(|symbol| symbol.id.name == property_name)
                .collect::<Vec<_>>();
            let [property] = matching.as_slice() else {
                continue;
            };
            let Some(property_source) = provider.source_for_file(&property.id.file) else {
                continue;
            };
            if property.location.file != property.id.file
                || property.location.snapshot_hash
                    != *blake3::hash(property_source.as_bytes()).as_bytes()
            {
                continue;
            }
            let FactStatus::Established(source_type) = &property.ty else {
                continue;
            };
            let mut dependencies = Vec::new();
            let Some(mut result_type) = crate::semantics::qualify_type_in_file(
                source_type,
                &property.id.file,
                provider,
                &mut dependencies,
            ) else {
                continue;
            };
            let jvm_primitive = java_primitive_descriptor(&field_spelling).is_some();
            if jvm_primitive {
                result_type.nullable = false;
                if !java_primitive_matches_type(&field_spelling, &result_type) {
                    continue;
                }
            } else {
                result_type.nullable = nullability != ReferenceReturnNullability::NonNull;
            }
            if jvm_primitive && nullability == ReferenceReturnNullability::Nullable {
                continue;
            }
            let Some(descriptor) =
                field_descriptor(&field_spelling, &result_type, &property.id, provider)
            else {
                continue;
            };
            let Some(actual_field_descriptor) = java_field_descriptor(
                &field_spelling,
                owner,
                provider,
                &imports,
                &wildcard_imports,
            ) else {
                continue;
            };
            if descriptor != actual_field_descriptor {
                continue;
            }
            let target =
                SymbolId::generated(&property.id, &format!("lombok:getter:{method_name}()"));
            contracts.push(GeneratedGetterContract {
                output_path: PathBuf::from(&file.path),
                output_hash,
                owner: (*owner).clone(),
                origin: property.id.clone(),
                target,
                property_name,
                method_name: method_name.clone(),
                result_type,
                jvm_signature: JvmSignature {
                    name: method_name,
                    descriptor: format!("(){descriptor}"),
                },
                jvm_primitive,
                nullability: if jvm_primitive {
                    ReferenceReturnNullability::NonNull
                } else {
                    nullability
                },
                dependencies,
            });
        }
    }
    contracts
}

/// Collect Lombok getter contracts from an indexed Java source snapshot.
/// This is conditional metadata: callers must independently prove that
/// Lombok is active and that no applicable `lombok.config` changes its
/// accessor rules.
pub(crate) fn collect_java_source(
    path: &Path,
    source: &str,
    provider: &dyn SemanticProvider,
) -> Vec<GeneratedGetterContract> {
    if provider.source_for_file(path) != Some(source) {
        return Vec::new();
    }
    let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
    let Ok(unit) = crate::java_ir::parse_java(source) else {
        return Vec::new();
    };
    if has_static_import(&unit.root) {
        return Vec::new();
    }
    let imports = lombok_imports(&unit.root);
    let wildcard_imports = wildcard_imports(&unit.root);
    let indexed_symbols = provider.symbols_in_file(path);
    let mut contracts = Vec::new();
    let mut seen_owners = std::collections::BTreeSet::new();

    for node in unit
        .root
        .children
        .iter()
        .filter_map(element_node)
        .filter(|node| node.kind == "class_declaration")
    {
        let Some(class_name) = declaration_name(node) else {
            continue;
        };
        if direct_child_kind(node, "type_parameters").is_some() {
            continue;
        }
        let owner_symbols = indexed_symbols
            .iter()
            .filter(|symbol| {
                symbol.id.file == path
                    && symbol.id.kind == "class"
                    && symbol.id.owner_path.is_empty()
                    && symbol.id.name == class_name
                    && symbol.location.snapshot_hash == source_hash
            })
            .collect::<Vec<_>>();
        let [owner_symbol] = owner_symbols.as_slice() else {
            continue;
        };
        let owner = &owner_symbol.id;
        if !seen_owners.insert(owner.clone()) {
            continue;
        }
        let Some(marker) = getter_marker(node, &imports) else {
            continue;
        };
        let Some(body) = field(node, "body") else {
            continue;
        };
        for candidate in eligible_fields(body, marker, &imports) {
            let mut dependencies = vec![owner.clone()];
            let origin = SymbolId {
                module: owner.module.clone(),
                package: owner.package.clone(),
                file: owner.file.clone(),
                owner_path: vec![class_name.clone()],
                kind: "field".into(),
                name: candidate.property_name.clone(),
                receiver: None,
                parameters: Vec::new(),
            };
            dependencies.push(origin.clone());

            let primitive = java_primitive_descriptor(&candidate.field_spelling).is_some();
            let (mut result_type, result_descriptor) = if primitive {
                let result_type = TypeRef::from_java_name(&candidate.field_spelling);
                let Some(descriptor) = java_primitive_descriptor(&candidate.field_spelling) else {
                    continue;
                };
                (result_type, descriptor.to_owned())
            } else {
                let Some(binding) = java_field_binding(
                    &candidate.field_spelling,
                    owner,
                    provider,
                    &imports,
                    &wildcard_imports,
                ) else {
                    continue;
                };
                if let Some(symbol) = &binding.symbol {
                    if provider.is_ordinary_jvm_reference(symbol) != FactStatus::Established(true) {
                        continue;
                    }
                    dependencies.push(symbol.clone());
                }
                let descriptor = format!("L{};", binding.binary_name.replace('.', "/"));
                let mut result_type = java_source_type_ref(&binding.source_name);
                result_type.nullable = candidate.nullability != ReferenceReturnNullability::NonNull;
                (result_type, descriptor)
            };
            if primitive {
                result_type.nullable = false;
            } else if candidate.nullability == ReferenceReturnNullability::Nullable {
                result_type.nullable = true;
            }
            if primitive && candidate.nullability == ReferenceReturnNullability::Nullable {
                continue;
            }
            dependencies.sort();
            dependencies.dedup();
            let target = SymbolId::generated(
                &origin,
                &format!("lombok:getter:{}()", candidate.method_name),
            );
            contracts.push(GeneratedGetterContract {
                output_path: path.to_path_buf(),
                output_hash: source_hash,
                owner: owner.clone(),
                origin,
                target,
                property_name: candidate.property_name,
                method_name: candidate.method_name.clone(),
                result_type,
                jvm_signature: JvmSignature {
                    name: candidate.method_name,
                    descriptor: format!("(){result_descriptor}"),
                },
                jvm_primitive: primitive,
                nullability: if primitive {
                    ReferenceReturnNullability::NonNull
                } else {
                    candidate.nullability
                },
                dependencies,
            });
        }
    }
    contracts
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GetterMarker {
    Getter,
    Data,
    Value,
}

fn getter_marker(
    class: &JavaSyntaxNode,
    imports: &BTreeMap<String, String>,
) -> Option<GetterMarker> {
    let modifiers = field_or_kind(class, "modifiers")?;
    let mut marker = None;
    for annotation in modifiers
        .children
        .iter()
        .filter_map(element_node)
        .filter(|node| matches!(node.kind.as_str(), "annotation" | "marker_annotation"))
    {
        let name = annotation_name(annotation)?;
        let canonical = resolve_annotation(&name, imports)?;
        let simple = canonical.rsplit('.').next()?;
        if let Some(current) = match canonical.as_str() {
            "lombok.Getter" => Some(GetterMarker::Getter),
            "lombok.Data" => Some(GetterMarker::Data),
            "lombok.Value" => Some(GetterMarker::Value),
            "lombok.NoArgsConstructor"
            | "lombok.RequiredArgsConstructor"
            | "lombok.AllArgsConstructor"
            | "lombok.EqualsAndHashCode"
            | "lombok.ToString" => None,
            _ => None,
        } {
            if marker.replace(current).is_some() {
                return None;
            }
        } else if !matches!(
            simple,
            "NoArgsConstructor"
                | "RequiredArgsConstructor"
                | "AllArgsConstructor"
                | "EqualsAndHashCode"
                | "ToString"
        ) {
            return None;
        }
        if field(annotation, "arguments").is_some() {
            return None;
        }
    }
    marker
}

struct GetterFieldCandidate {
    field_spelling: String,
    property_name: String,
    method_name: String,
    nullability: ReferenceReturnNullability,
}

fn eligible_fields(
    body: &JavaSyntaxNode,
    marker: GetterMarker,
    imports: &BTreeMap<String, String>,
) -> Vec<GetterFieldCandidate> {
    let mut field_name_counts = BTreeMap::<String, usize>::new();
    let members = body
        .children
        .iter()
        .filter_map(element_node)
        .collect::<Vec<_>>();
    for member in members
        .iter()
        .filter(|member| member.kind == "field_declaration")
    {
        let mut names = Vec::new();
        collect_variable_names(member, &mut names);
        for name in names {
            *field_name_counts
                .entry(name.to_ascii_lowercase())
                .or_default() += 1;
        }
    }
    let collisions = field_name_counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect::<std::collections::BTreeSet<_>>();
    let mut candidates = Vec::new();
    for member in members
        .into_iter()
        .filter(|member| member.kind == "field_declaration")
    {
        let modifiers = field_or_kind(member, "modifiers");
        if modifiers.is_none() && marker != GetterMarker::Value {
            continue;
        }
        if modifiers.is_some_and(|modifiers| has_token(modifiers, "static")) {
            continue;
        }
        if marker != GetterMarker::Value
            && !modifiers.is_some_and(|modifiers| has_token(modifiers, "private"))
        {
            continue;
        }
        if marker == GetterMarker::Value
            && modifiers.is_some_and(|modifiers| {
                has_token(modifiers, "public") || has_token(modifiers, "protected")
            })
        {
            continue;
        }
        let nullability = modifiers
            .map(|modifiers| field_nullability(modifiers, imports))
            .unwrap_or(Some(ReferenceReturnNullability::Platform));
        let Some(nullability) = nullability else {
            continue;
        };
        let Some(field_type_node) = field(member, "type") else {
            continue;
        };
        if contains_generic_or_array(field_type_node) {
            continue;
        }
        let field_spelling = render(&crate::java_ir::JavaCompilationUnit {
            root: field_type_node.clone(),
        })
        .trim()
        .to_owned();
        let Some(property_name) = field_property_name(member) else {
            continue;
        };
        if !ordinary_property_name(&property_name)
            || field_spelling == "boolean"
            || property_name.starts_with("is")
            || collisions.contains(&property_name.to_ascii_lowercase())
        {
            continue;
        }
        let method_name = format!(
            "get{}{}",
            property_name[..1].to_ascii_uppercase(),
            &property_name[1..]
        );
        if class_has_getter_suppression(body, &method_name) {
            continue;
        }
        candidates.push(GetterFieldCandidate {
            field_spelling,
            property_name,
            method_name,
            nullability,
        });
    }
    candidates
}

fn field_nullability(
    modifiers: &JavaSyntaxNode,
    imports: &BTreeMap<String, String>,
) -> Option<ReferenceReturnNullability> {
    let mut result = None;
    for annotation in modifiers
        .children
        .iter()
        .filter_map(element_node)
        .filter(|node| matches!(node.kind.as_str(), "annotation" | "marker_annotation"))
    {
        let name = annotation_name(annotation)?;
        let canonical = resolve_annotation(&name, imports)?;
        let current = match canonical.as_str() {
            "lombok.NonNull" => Some(ReferenceReturnNullability::NonNull),
            "org.jetbrains.annotations.NotNull" => Some(ReferenceReturnNullability::NonNull),
            "org.jetbrains.annotations.Nullable" => Some(ReferenceReturnNullability::Nullable),
            _ => None,
        };
        if field(annotation, "arguments").is_some() {
            return None;
        }
        if let Some(current) = current {
            if result.is_some_and(|previous| previous != current) {
                return None;
            }
            result = Some(current);
        }
    }
    Some(result.unwrap_or(ReferenceReturnNullability::Platform))
}

fn lombok_imports(unit: &JavaSyntaxNode) -> BTreeMap<String, String> {
    let mut imports: BTreeMap<String, String> = BTreeMap::new();
    for import in unit
        .children
        .iter()
        .filter_map(element_node)
        .filter(|node| node.kind == "import_declaration")
    {
        let text = render(&crate::java_ir::JavaCompilationUnit {
            root: import.clone(),
        });
        let text = text
            .trim()
            .trim_start_matches("import ")
            .trim_end_matches(';');
        if text.starts_with("static ") || text.ends_with(".*") {
            continue;
        }
        if let Some((_, simple)) = text.rsplit_once('.') {
            imports
                .entry(simple.to_owned())
                .and_modify(|existing| {
                    if existing != text {
                        existing.clear();
                    }
                })
                .or_insert_with(|| text.to_owned());
        }
    }
    imports
}

fn resolve_annotation(name: &str, imports: &BTreeMap<String, String>) -> Option<String> {
    if name.starts_with("lombok.") || name.starts_with("org.jetbrains.annotations.") {
        return Some(name.to_owned());
    }
    imports.get(name).cloned()
}

fn annotation_name(node: &JavaSyntaxNode) -> Option<String> {
    field(node, "name").map(render_node)
}

fn field_property_name(node: &JavaSyntaxNode) -> Option<String> {
    let mut names = Vec::new();
    collect_variable_names(node, &mut names);
    match names.as_slice() {
        [name] => Some(name.clone()),
        _ => None,
    }
}

fn collect_variable_names(node: &JavaSyntaxNode, names: &mut Vec<String>) {
    if node.kind == "variable_declarator" {
        if let Some(name) = field(node, "name").map(render_node) {
            names.push(name);
        }
        return;
    }
    for child in node.children.iter().filter_map(element_node) {
        collect_variable_names(child, names);
    }
}

fn class_has_getter_suppression(body: &JavaSyntaxNode, getter: &str) -> bool {
    // Lombok suppresses a getter when an existing same-name method can be
    // called with zero arguments; a varargs-only method qualifies. See
    // https://projectlombok.org/features/GetterSetter.
    body.children.iter().filter_map(element_node).any(|member| {
        member.kind == "method_declaration"
            && declaration_name(member).is_some_and(|name| name.eq_ignore_ascii_case(getter))
            && !field(member, "parameters")
                .or_else(|| direct_child_kind(member, "formal_parameters"))
                .is_some_and(|parameters| {
                    parameters
                        .children
                        .iter()
                        .filter_map(element_node)
                        .any(|parameter| parameter.kind == "formal_parameter")
                })
    })
}

fn ordinary_property_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn contains_generic_or_array(node: &JavaSyntaxNode) -> bool {
    if matches!(node.kind.as_str(), "generic_type" | "array_type") {
        return true;
    }
    node.children
        .iter()
        .filter_map(element_node)
        .any(contains_generic_or_array)
}

fn field_descriptor(
    field_type: &str,
    result_type: &TypeRef,
    origin: &SymbolId,
    provider: &dyn SemanticProvider,
) -> Option<String> {
    if let Some(descriptor) = java_primitive_descriptor(field_type) {
        return Some(descriptor.into());
    }
    if let KotlinType::Primitive(name) = &result_type.ty {
        if result_type.nullable {
            let boxed = match name.as_str() {
                "Boolean" => "java.lang.Boolean",
                "Byte" => "java.lang.Byte",
                "Short" => "java.lang.Short",
                "Int" => "java.lang.Integer",
                "Long" => "java.lang.Long",
                "Char" => "java.lang.Character",
                "Float" => "java.lang.Float",
                "Double" => "java.lang.Double",
                _ => return None,
            };
            return Some(format!("L{};", boxed.replace('.', "/")));
        }
        return None;
    }
    let KotlinType::Named { name, arguments } = &result_type.ty else {
        return None;
    };
    if !arguments.is_empty() {
        return None;
    }
    let canonical = match name.as_str() {
        "String" | "kotlin.String" | "java.lang.String" => "java.lang.String".to_owned(),
        "java.math.BigDecimal" => "java.math.BigDecimal".to_owned(),
        "java.math.BigInteger" => "java.math.BigInteger".to_owned(),
        "java.util.UUID" => "java.util.UUID".to_owned(),
        "java.lang.Boolean" | "Boolean" => "java.lang.Boolean".to_owned(),
        "java.lang.Byte" | "Byte" => "java.lang.Byte".to_owned(),
        "java.lang.Short" | "Short" => "java.lang.Short".to_owned(),
        "java.lang.Integer" | "Int" => "java.lang.Integer".to_owned(),
        "java.lang.Long" | "Long" => "java.lang.Long".to_owned(),
        "java.lang.Character" | "Char" => "java.lang.Character".to_owned(),
        "java.lang.Float" | "Float" => "java.lang.Float".to_owned(),
        "java.lang.Double" | "Double" => "java.lang.Double".to_owned(),
        _ => match provider.resolve_type_in_file(&origin.file, &origin.package, name) {
            FactStatus::Established(symbol) => symbol_qualified_name(&symbol),
            FactStatus::Unknown | FactStatus::Inferred(_) | FactStatus::Ambiguous(_) => {
                return None;
            }
        },
    };
    Some(format!("L{};", canonical.replace('.', "/")))
}

fn java_field_descriptor(
    field_type: &str,
    owner: &SymbolId,
    provider: &dyn SemanticProvider,
    imports: &BTreeMap<String, String>,
    wildcard_imports: &std::collections::BTreeSet<String>,
) -> Option<String> {
    if let Some(descriptor) = java_primitive_descriptor(field_type) {
        return Some(descriptor.into());
    }
    let binding = java_field_binding(field_type, owner, provider, imports, wildcard_imports)?;
    Some(format!("L{};", binding.binary_name.replace('.', "/")))
}

struct JavaFieldBinding {
    source_name: String,
    binary_name: String,
    symbol: Option<SymbolId>,
}

fn java_field_binding(
    field_type: &str,
    owner: &SymbolId,
    provider: &dyn SemanticProvider,
    imports: &BTreeMap<String, String>,
    wildcard_imports: &std::collections::BTreeSet<String>,
) -> Option<JavaFieldBinding> {
    if field_type.contains('.') {
        let simple = field_type.rsplit('.').next()?;
        let matching_types = provider
            .symbols_named(simple)
            .into_iter()
            .map(|symbol| &symbol.id)
            .filter(|symbol| {
                is_type_symbol(symbol)
                    && (imported_type_matches(field_type, symbol)
                        || imported_type_matches(
                            &format!("{}.{}", owner.package, field_type),
                            symbol,
                        ))
            })
            .collect::<Vec<_>>();
        match matching_types.as_slice() {
            [one] => Some(java_field_binding_for_symbol(one)),
            [] => {
                let external = known_external_java_type(field_type)?.to_owned();
                Some(JavaFieldBinding {
                    source_name: external.clone(),
                    binary_name: external,
                    symbol: None,
                })
            }
            _ => None,
        }
    } else if let Some(imported) = imports.get(field_type) {
        if imported.is_empty() {
            return None;
        }
        if provider.symbols_named(field_type).iter().any(|symbol| {
            is_type_symbol(&symbol.id)
                && symbol.id.package == owner.package
                && symbol.id.owner_path.is_empty()
                && !imported_type_matches(imported, &symbol.id)
        }) {
            return None;
        }
        let simple = imported.rsplit('.').next()?;
        let matching_types = provider
            .symbols_named(simple)
            .into_iter()
            .map(|symbol| &symbol.id)
            .filter(|symbol| is_type_symbol(symbol) && imported_type_matches(imported, symbol))
            .collect::<Vec<_>>();
        match matching_types.as_slice() {
            [one] => Some(java_field_binding_for_symbol(one)),
            [] => {
                let external = known_external_java_type(imported)?.to_owned();
                Some(JavaFieldBinding {
                    source_name: external.clone(),
                    binary_name: external,
                    symbol: None,
                })
            }
            _ => None,
        }
    } else {
        let same_package = provider
            .symbols_named(field_type)
            .into_iter()
            .map(|symbol| &symbol.id)
            .filter(|symbol| {
                is_type_symbol(symbol)
                    && symbol.package == owner.package
                    && symbol.owner_path.is_empty()
                    && symbol.name == field_type
            })
            .collect::<Vec<_>>();
        match same_package.as_slice() {
            [one] => Some(java_field_binding_for_symbol(one)),
            [] => {
                if !wildcard_imports.is_empty() {
                    let known_platform_type = java_lang_type(field_type).is_some();
                    let only_known_platform_wildcards = wildcard_imports
                        .iter()
                        .all(|package| matches!(package.as_str(), "java.util" | "java.lang"));
                    let wildcard_type_collision =
                        provider.symbols_named(field_type).iter().any(|symbol| {
                            is_type_symbol(&symbol.id)
                                && symbol.id.owner_path.is_empty()
                                && wildcard_imports.contains(&symbol.id.package)
                        });
                    if !(known_platform_type
                        && only_known_platform_wildcards
                        && !wildcard_type_collision)
                    {
                        return None;
                    }
                }
                let platform = java_lang_type(field_type)?.to_owned();
                Some(JavaFieldBinding {
                    source_name: platform.clone(),
                    binary_name: platform,
                    symbol: None,
                })
            }
            _ => None,
        }
    }
}

fn java_field_binding_for_symbol(symbol: &SymbolId) -> JavaFieldBinding {
    let mut source_names = symbol.owner_path.clone();
    source_names.push(symbol.name.clone());
    let source_name = if symbol.package.is_empty() {
        source_names.join(".")
    } else {
        format!("{}.{}", symbol.package, source_names.join("."))
    };
    JavaFieldBinding {
        source_name,
        binary_name: symbol_qualified_name(symbol),
        symbol: Some(symbol.clone()),
    }
}

fn java_source_type_ref(source_name: &str) -> TypeRef {
    if source_name == "java.lang.String" {
        return TypeRef {
            ty: KotlinType::Named {
                name: source_name.to_owned(),
                arguments: Vec::new(),
            },
            nullable: false,
        };
    }
    TypeRef::from_java_name(source_name)
}

fn wildcard_imports(unit: &JavaSyntaxNode) -> std::collections::BTreeSet<String> {
    unit.children
        .iter()
        .filter_map(element_node)
        .filter(|node| node.kind == "import_declaration")
        .filter_map(|import| {
            let text = render(&crate::java_ir::JavaCompilationUnit {
                root: import.clone(),
            });
            let text = text
                .trim()
                .trim_start_matches("import ")
                .trim_end_matches(';');
            (!text.starts_with("static ") && text.ends_with(".*"))
                .then(|| text.trim_end_matches(".*").to_owned())
        })
        .collect()
}

fn has_static_import(unit: &JavaSyntaxNode) -> bool {
    unit.children
        .iter()
        .filter_map(element_node)
        .filter(|node| node.kind == "import_declaration")
        .any(|import| {
            let text = render(&crate::java_ir::JavaCompilationUnit {
                root: import.clone(),
            });
            text.trim()
                .trim_start_matches("import ")
                .starts_with("static ")
        })
}

fn is_type_symbol(symbol: &SymbolId) -> bool {
    matches!(
        symbol.kind.as_str(),
        "class" | "interface" | "enum" | "annotation" | "record" | "object"
    )
}

fn imported_type_matches(imported: &str, symbol: &SymbolId) -> bool {
    let direct = symbol_qualified_name(symbol);
    let dotted_nested = if symbol.owner_path.is_empty() {
        direct.clone()
    } else {
        format!(
            "{}.{}",
            symbol.package,
            std::iter::once(&symbol.owner_path[0])
                .chain(symbol.owner_path.iter().skip(1))
                .chain(std::iter::once(&symbol.name))
                .cloned()
                .collect::<Vec<_>>()
                .join(".")
        )
    };
    imported == direct || imported == dotted_nested
}

fn known_external_java_type(name: &str) -> Option<&'static str> {
    match name {
        "java.lang.String" => Some("java.lang.String"),
        "java.lang.Boolean" => Some("java.lang.Boolean"),
        "java.lang.Byte" => Some("java.lang.Byte"),
        "java.lang.Short" => Some("java.lang.Short"),
        "java.lang.Integer" => Some("java.lang.Integer"),
        "java.lang.Long" => Some("java.lang.Long"),
        "java.lang.Character" => Some("java.lang.Character"),
        "java.lang.Float" => Some("java.lang.Float"),
        "java.lang.Double" => Some("java.lang.Double"),
        "java.math.BigDecimal" => Some("java.math.BigDecimal"),
        "java.math.BigInteger" => Some("java.math.BigInteger"),
        "java.util.UUID" => Some("java.util.UUID"),
        "java.time.Instant" => Some("java.time.Instant"),
        "java.util.List" => Some("java.util.List"),
        "java.util.Set" => Some("java.util.Set"),
        "java.util.Map" => Some("java.util.Map"),
        "java.util.Collection" => Some("java.util.Collection"),
        "java.util.Optional" => Some("java.util.Optional"),
        "java.util.HashMap" => Some("java.util.HashMap"),
        "java.util.stream.Stream" => Some("java.util.stream.Stream"),
        _ => None,
    }
}

fn java_lang_type(name: &str) -> Option<&'static str> {
    match name {
        "String" => Some("java.lang.String"),
        "Boolean" => Some("java.lang.Boolean"),
        "Byte" => Some("java.lang.Byte"),
        "Short" => Some("java.lang.Short"),
        "Integer" => Some("java.lang.Integer"),
        "Long" => Some("java.lang.Long"),
        "Character" => Some("java.lang.Character"),
        "Float" => Some("java.lang.Float"),
        "Double" => Some("java.lang.Double"),
        "Object" => Some("java.lang.Object"),
        _ => None,
    }
}

fn symbol_qualified_name(symbol: &SymbolId) -> String {
    let mut names = symbol.owner_path.clone();
    names.push(symbol.name.clone());
    if symbol.package.is_empty() {
        names.join("$")
    } else {
        format!("{}.{}", symbol.package, names.join("$"))
    }
}

fn java_primitive_descriptor(name: &str) -> Option<&'static str> {
    match name {
        "boolean" => Some("Z"),
        "byte" => Some("B"),
        "char" => Some("C"),
        "short" => Some("S"),
        "int" => Some("I"),
        "long" => Some("J"),
        "float" => Some("F"),
        "double" => Some("D"),
        _ => None,
    }
}

fn java_primitive_matches_type(field_type: &str, ty: &TypeRef) -> bool {
    let KotlinType::Primitive(source_name) = &ty.ty else {
        return false;
    };
    let expected = match source_name.as_str() {
        "Boolean" => "boolean",
        "Byte" => "byte",
        "Char" => "char",
        "Short" => "short",
        "Int" => "int",
        "Long" => "long",
        "Float" => "float",
        "Double" => "double",
        _ => return false,
    };
    field_type == expected
}

fn has_token(node: &JavaSyntaxNode, expected: &str) -> bool {
    node.children.iter().any(|child| match child {
        JavaElement::Token(token) => token.text == expected,
        _ => element_node(child).is_some_and(|nested| has_token(nested, expected)),
    })
}

fn declaration_name(node: &JavaSyntaxNode) -> Option<String> {
    field(node, "name").map(render_node)
}

fn field<'a>(node: &'a JavaSyntaxNode, name: &str) -> Option<&'a JavaSyntaxNode> {
    node.children.iter().find_map(|child| {
        let nested = element_node(child)?;
        (nested.field_name.as_deref() == Some(name)).then_some(nested)
    })
}

fn field_or_kind<'a>(node: &'a JavaSyntaxNode, kind: &str) -> Option<&'a JavaSyntaxNode> {
    field(node, kind).or_else(|| direct_child_kind(node, kind))
}

fn direct_child_kind<'a>(node: &'a JavaSyntaxNode, kind: &str) -> Option<&'a JavaSyntaxNode> {
    node.children
        .iter()
        .filter_map(element_node)
        .find(|child| child.kind == kind)
}

fn render_node(node: &JavaSyntaxNode) -> String {
    render(&crate::java_ir::JavaCompilationUnit { root: node.clone() })
        .trim()
        .to_owned()
}

fn element_node(element: &JavaElement) -> Option<&JavaSyntaxNode> {
    match element {
        JavaElement::Declaration(node)
        | JavaElement::Type(node)
        | JavaElement::Expression(node)
        | JavaElement::Statement(node)
        | JavaElement::Annotation(node)
        | JavaElement::Import(node)
        | JavaElement::Other(node) => Some(node),
        JavaElement::Token(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantics::{FactStatus, SemanticSymbol, SourceLocation};

    struct TestProvider {
        source_path: PathBuf,
        source: String,
        symbols: Vec<SemanticSymbol>,
        types: Vec<SymbolId>,
    }

    impl SemanticProvider for TestProvider {
        fn resolve(&self, _name: &str) -> FactStatus<SymbolId> {
            FactStatus::Unknown
        }

        fn symbols(&self) -> &[SemanticSymbol] {
            &self.symbols
        }

        fn source_for_file(&self, file: &std::path::Path) -> Option<&str> {
            (file == self.source_path).then_some(self.source.as_str())
        }

        fn resolve_type_in_file(
            &self,
            _file: &std::path::Path,
            _package: &str,
            name: &str,
        ) -> FactStatus<SymbolId> {
            let candidates = self
                .types
                .iter()
                .filter(|ty| ty.name == name || symbol_qualified_name(ty) == name)
                .cloned()
                .collect::<Vec<_>>();
            match candidates.as_slice() {
                [one] => FactStatus::Established(one.clone()),
                [] => FactStatus::Unknown,
                _ => FactStatus::Ambiguous(candidates),
            }
        }

        fn is_ordinary_jvm_reference(&self, owner: &SymbolId) -> FactStatus<bool> {
            FactStatus::Established(!owner.name.starts_with("Value"))
        }
    }

    fn symbol(file: &std::path::Path, owner_path: Vec<String>, kind: &str, name: &str) -> SymbolId {
        SymbolId {
            module: "fixture".into(),
            package: "sample".into(),
            file: file.to_path_buf(),
            owner_path,
            kind: kind.into(),
            name: name.into(),
            receiver: None,
            parameters: Vec::new(),
        }
    }

    fn fixture(
        java: &str,
        kotlin: &str,
        properties: &[(&str, &str)],
        extra_types: Vec<SymbolId>,
    ) -> (AcceptedJavaFile, TestProvider) {
        let source_path = PathBuf::from("src/Box.kt");
        let owner = symbol(&source_path, Vec::new(), "class", "Box");
        let mut symbols = Vec::new();
        for (name, ty) in properties {
            symbols.push(SemanticSymbol {
                id: symbol(&source_path, vec!["Box".into()], "property", name),
                location: SourceLocation {
                    file: source_path.clone(),
                    snapshot_hash: *blake3::hash(kotlin.as_bytes()).as_bytes(),
                    start_byte: 0,
                    end_byte: 1,
                },
                ty: FactStatus::Established(TypeRef::parse(ty)),
            });
        }
        let unit = crate::java_ir::parse_java(java).expect("fixture Java parses");
        (
            AcceptedJavaFile {
                path: "src/Box.java".into(),
                unit,
                owners: vec![owner],
            },
            TestProvider {
                source_path,
                source: kotlin.into(),
                symbols,
                types: extra_types,
            },
        )
    }

    fn java_source_provider(
        path: &std::path::Path,
        source: &str,
        top_level_types: &[&str],
        types: Vec<SymbolId>,
    ) -> TestProvider {
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        let mut symbols = Vec::new();
        for name in top_level_types {
            let id = symbol(path, Vec::new(), "class", name);
            symbols.push(SemanticSymbol {
                id,
                location: SourceLocation {
                    file: path.to_path_buf(),
                    snapshot_hash: hash,
                    start_byte: 0,
                    end_byte: source.len(),
                },
                ty: FactStatus::Unknown,
            });
        }
        for id in &types {
            if symbols.iter().any(|symbol| symbol.id == *id) {
                continue;
            }
            symbols.push(SemanticSymbol {
                id: id.clone(),
                location: SourceLocation {
                    file: id.file.clone(),
                    snapshot_hash: hash,
                    start_byte: 0,
                    end_byte: source.len(),
                },
                ty: FactStatus::Unknown,
            });
        }
        TestProvider {
            source_path: path.to_path_buf(),
            source: source.to_owned(),
            symbols,
            types,
        }
    }

    #[test]
    fn recognizes_getter_data_and_value_with_exact_field_descriptors() {
        let (file, provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private int count; }",
            "package sample\nclass Box(val count: Int)",
            &[("count", "Int")],
            Vec::new(),
        );
        let found = collect(&file, &provider);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].method_name, "getCount");
        assert_eq!(found[0].jvm_signature.descriptor, "()I");
        assert!(found[0].jvm_primitive);

        let (file, provider) = fixture(
            "package sample; import lombok.Getter; import lombok.NonNull; @Getter class Box { @NonNull private String label; }",
            "package sample\nclass Box(val label: String)",
            &[("label", "String")],
            Vec::new(),
        );
        let found = collect(&file, &provider);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].nullability, ReferenceReturnNullability::NonNull);
        assert!(!found[0].result_type.nullable);

        let (file, provider) = fixture(
            "package sample; import lombok.Data; @Data class Box { private String label; }",
            "package sample\nclass Box(val label: String?)",
            &[("label", "String?")],
            Vec::new(),
        );
        let found = collect(&file, &provider);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].jvm_signature.descriptor, "()Ljava/lang/String;");
        assert_eq!(found[0].nullability, ReferenceReturnNullability::Platform);
        assert!(found[0].result_type.nullable);

        let (file, provider) = fixture(
            "package sample; import lombok.Value; @Value class Box { int count; }",
            "package sample\nclass Box(val count: Int)",
            &[("count", "Int")],
            Vec::new(),
        );
        let found = collect(&file, &provider);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].jvm_signature.descriptor, "()I");
    }

    #[test]
    fn nullable_boxed_primitive_and_imported_reference_keep_jvm_identity() {
        let (file, provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private Integer count; }",
            "package sample\nclass Box(val count: Int?)",
            &[("count", "Int?")],
            Vec::new(),
        );
        let found = collect(&file, &provider);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].jvm_signature.descriptor, "()Ljava/lang/Integer;");
        assert_eq!(found[0].nullability, ReferenceReturnNullability::Platform);
        assert!(found[0].result_type.nullable);

        let (file, provider) = fixture(
            "package sample; import lombok.Getter; import org.jetbrains.annotations.Nullable; @Getter class Box { @Nullable private Integer count; }",
            "package sample\nclass Box(val count: Int?)",
            &[("count", "Int?")],
            Vec::new(),
        );
        let found = collect(&file, &provider);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].jvm_signature.descriptor, "()Ljava/lang/Integer;");
        assert_eq!(found[0].nullability, ReferenceReturnNullability::Nullable);

        let payload = SymbolId {
            module: "fixture".into(),
            package: "sample.types".into(),
            file: PathBuf::from("src/Payload.kt"),
            owner_path: Vec::new(),
            kind: "class".into(),
            name: "Payload".into(),
            receiver: None,
            parameters: Vec::new(),
        };
        let (file, mut provider) = fixture(
            "package sample; import lombok.Getter; import sample.types.Payload; @Getter class Box { private Payload payload; }",
            "package sample\nimport sample.types.Payload\nclass Box(val payload: Payload)",
            &[("payload", "Payload")],
            vec![payload.clone()],
        );
        provider.symbols.push(SemanticSymbol {
            id: payload.clone(),
            location: SourceLocation {
                file: payload.file.clone(),
                snapshot_hash: [2; 32],
                start_byte: 0,
                end_byte: 1,
            },
            ty: FactStatus::Unknown,
        });
        let found = collect(&file, &provider);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].jvm_signature.descriptor,
            "()Lsample/types/Payload;"
        );
        assert!(found[0].dependencies.contains(&payload));

        let (file, provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private Payload payload; }",
            "package sample\nimport sample.types.Payload\nclass Box(val payload: Payload)",
            &[("payload", "Payload")],
            vec![payload.clone()],
        );
        assert!(collect(&file, &provider).is_empty());

        let (file, provider) = fixture(
            "package sample; import lombok.Getter; import other.Payload; @Getter class Box { private Payload payload; }",
            "package sample\nimport sample.types.Payload\nclass Box(val payload: Payload)",
            &[("payload", "Payload")],
            vec![payload],
        );
        assert!(collect(&file, &provider).is_empty());
    }

    #[test]
    fn rejects_mismatched_or_suppressed_getters_and_nonordinary_classes() {
        let cases = [
            (
                "package sample; import lombok.Getter; @Getter class Box { private long count; }",
                "package sample\nclass Box(val count: Int)",
            ),
            (
                "package sample; import lombok.Getter; @Getter class Box { private int count; public int GETCOUNT() { return count; } }",
                "package sample\nclass Box(val count: Int)",
            ),
            (
                "package sample; import lombok.Getter; @Getter class Box<T> { private int count; }",
                "package sample\nclass Box<T>(val count: Int)",
            ),
            (
                "package sample; import lombok.Getter; import lombok.experimental.Accessors; @Getter @Accessors(fluent=true) class Box { private int count; }",
                "package sample\nclass Box(val count: Int)",
            ),
            (
                "package sample; import lombok.Getter; @Getter class Box { @Getter(AccessLevel.NONE) private int count; }",
                "package sample\nclass Box(val count: Int)",
            ),
            (
                "package sample; import lombok.Value; @Value class Box { public int count; }",
                "package sample\nclass Box(val count: Int)",
            ),
        ];
        for (java, kotlin) in cases {
            let (file, provider) = fixture(java, kotlin, &[("count", "Int")], Vec::new());
            assert!(
                collect(&file, &provider).is_empty(),
                "unexpected contract for {java}"
            );
        }
    }

    #[test]
    fn varargs_only_method_suppresses_getter_but_required_parameter_does_not() {
        let (file, provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private int count; public int getCount(String... args) { return count; } }",
            "package sample\nclass Box(val count: Int)",
            &[("count", "Int")],
            Vec::new(),
        );
        assert!(collect(&file, &provider).is_empty());

        let (file, provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private int count; public int getCount(int value, String... args) { return count + value; } }",
            "package sample\nclass Box(val count: Int)",
            &[("count", "Int")],
            Vec::new(),
        );
        let contracts = collect(&file, &provider);
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].method_name, "getCount");
    }

    #[test]
    fn case_insensitive_field_names_cannot_authorize_lombok_getter_contracts() {
        let (file, provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private int fOo; private int foo; }",
            "package sample\nclass Box(val foo: Int)",
            &[("foo", "Int")],
            Vec::new(),
        );
        assert!(collect(&file, &provider).is_empty());
    }

    #[test]
    fn ambiguous_lombok_imports_fail_closed() {
        let (file, provider) = fixture(
            "package sample; import lombok.Getter; import other.Getter; @Getter class Box { private int count; }",
            "package sample\nclass Box(val count: Int)",
            &[("count", "Int")],
            Vec::new(),
        );
        assert!(collect(&file, &provider).is_empty());
    }

    #[test]
    fn static_imports_fail_closed_for_type_and_annotation_binding() {
        for java in [
            "package sample; import lombok.Getter; import static other.Container.String; @Getter class Box { private String label; }",
            "package sample; import lombok.Getter; import static other.Container.*; @Getter class Box { private String label; }",
            "package sample; import static other.Container.Getter; @Getter class Box { private int count; }",
        ] {
            let (file, provider) = fixture(
                java,
                "package sample\nclass Box(val label: String, val count: Int)",
                &[("label", "String"), ("count", "Int")],
                Vec::new(),
            );
            assert!(
                collect(&file, &provider).is_empty(),
                "unexpected contract for {java}"
            );
        }
    }

    #[test]
    fn java_field_resolution_uses_java_package_scope_and_rejects_wildcards() {
        let (file, mut provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private String label; }",
            "package sample\nclass Box(val label: String)",
            &[("label", "String")],
            Vec::new(),
        );
        let shadow = SymbolId {
            module: "fixture".into(),
            package: "sample".into(),
            file: PathBuf::from("src/String.kt"),
            owner_path: Vec::new(),
            kind: "class".into(),
            name: "String".into(),
            receiver: None,
            parameters: Vec::new(),
        };
        provider.types.push(shadow.clone());
        provider.symbols.push(SemanticSymbol {
            id: shadow.clone(),
            location: SourceLocation {
                file: shadow.file.clone(),
                snapshot_hash: [3; 32],
                start_byte: 0,
                end_byte: 1,
            },
            ty: FactStatus::Unknown,
        });
        assert_eq!(
            java_field_descriptor(
                "String",
                &file.owners[0],
                &provider,
                &BTreeMap::new(),
                &std::collections::BTreeSet::new()
            ),
            Some("Lsample/String;".into())
        );

        let (file, provider) = fixture(
            "package sample; import lombok.Getter; import sample.types.*; @Getter class Box { private Payload payload; }",
            "package sample\nimport sample.types.Payload\nclass Box(val payload: Payload)",
            &[("payload", "Payload")],
            vec![SymbolId {
                module: "fixture".into(),
                package: "sample.types".into(),
                file: PathBuf::from("src/Payload.kt"),
                owner_path: Vec::new(),
                kind: "class".into(),
                name: "Payload".into(),
                receiver: None,
                parameters: Vec::new(),
            }],
        );
        assert!(collect(&file, &provider).is_empty());

        // Generated Java commonly includes java.util.*. That known import is
        // harmless for java.lang types, while arbitrary wildcard packages
        // above remain unresolved and rejected.
        let (file, provider) = fixture(
            "package sample; import lombok.Getter; import java.util.*; import java.util.stream.Stream; @Getter class Box { private String label; }",
            "package sample\nclass Box(val label: String)",
            &[("label", "String")],
            Vec::new(),
        );
        let contracts = collect(&file, &provider);
        assert_eq!(contracts.len(), 1);
        assert_eq!(
            contracts[0].jvm_signature.descriptor,
            "()Ljava/lang/String;"
        );

        let (file, mut provider) = fixture(
            "package sample; import lombok.Getter; import java.util.*; @Getter class Box { private String label; }",
            "package sample\nclass Box(val label: String)",
            &[("label", "String")],
            Vec::new(),
        );
        let shadow = SymbolId {
            module: "fixture".into(),
            package: "java.util".into(),
            file: PathBuf::from("src/UtilString.kt"),
            owner_path: Vec::new(),
            kind: "class".into(),
            name: "String".into(),
            receiver: None,
            parameters: Vec::new(),
        };
        provider.types.push(shadow.clone());
        provider.symbols.push(SemanticSymbol {
            id: shadow.clone(),
            location: SourceLocation {
                file: shadow.file.clone(),
                snapshot_hash: [3; 32],
                start_byte: 0,
                end_byte: 1,
            },
            ty: FactStatus::Unknown,
        });
        assert!(collect(&file, &provider).is_empty());
    }

    #[test]
    fn stale_property_snapshot_cannot_authorize_a_generated_getter() {
        let (file, mut provider) = fixture(
            "package sample; import lombok.Getter; @Getter class Box { private int count; }",
            "package sample\nclass Box(val count: Int)",
            &[("count", "Int")],
            Vec::new(),
        );
        provider.symbols[0].location.snapshot_hash = [0; 32];
        assert!(collect(&file, &provider).is_empty());
    }

    #[test]
    fn java_source_contract_uses_raw_snapshot_and_field_origin_identity() {
        let path = PathBuf::from("src/Box.java");
        let source =
            "package sample;\nimport lombok.Getter;\n\n@Getter\nclass Box { private int count; }\n";
        let provider = java_source_provider(&path, source, &["Box"], Vec::new());
        let contracts = collect_java_source(&path, source, &provider);
        assert_eq!(contracts.len(), 1);
        let contract = &contracts[0];
        assert_eq!(contract.output_path, path);
        assert_eq!(
            contract.output_hash,
            *blake3::hash(source.as_bytes()).as_bytes()
        );
        assert_eq!(contract.origin.kind, "field");
        assert_eq!(contract.origin.file, path);
        assert_eq!(contract.origin.owner_path, ["Box"]);
        assert_eq!(contract.origin.name, "count");
        assert_eq!(contract.jvm_signature.descriptor, "()I");
        assert!(contract.jvm_primitive);
        assert!(contract.dependencies.contains(&contract.owner));
        assert!(contract.dependencies.contains(&contract.origin));

        let mut stale = provider;
        stale.source.push_str("// changed\n");
        assert!(collect_java_source(&path, source, &stale).is_empty());
    }

    #[test]
    fn java_source_boxed_nonnull_and_imported_nested_types_keep_java_abi() {
        let path = PathBuf::from("src/Box.java");
        let source = "package sample; import lombok.Getter; import lombok.NonNull; @Getter class Box { @NonNull private Integer count; private Outer.Payload payload; } class Outer { static class Payload {} }";
        let nested = SymbolId {
            module: "fixture".into(),
            package: "sample".into(),
            file: path.clone(),
            owner_path: vec!["Outer".into()],
            kind: "class".into(),
            name: "Payload".into(),
            receiver: None,
            parameters: Vec::new(),
        };
        let provider = java_source_provider(&path, source, &["Box", "Outer"], vec![nested.clone()]);
        let contracts = collect_java_source(&path, source, &provider);
        assert_eq!(contracts.len(), 2);
        let count = contracts
            .iter()
            .find(|contract| contract.property_name == "count")
            .unwrap();
        assert!(!count.jvm_primitive);
        assert_eq!(count.jvm_signature.descriptor, "()Ljava/lang/Integer;");
        assert_eq!(count.nullability, ReferenceReturnNullability::NonNull);
        assert!(!count.result_type.nullable);
        assert_eq!(count.result_type.ty, KotlinType::Primitive("Int".into()));

        let payload = contracts
            .iter()
            .find(|contract| contract.property_name == "payload")
            .unwrap();
        assert_eq!(payload.jvm_signature.descriptor, "()Lsample/Outer$Payload;");
        assert_eq!(
            payload.result_type.ty,
            KotlinType::Named {
                name: "sample.Outer.Payload".into(),
                arguments: Vec::new(),
            }
        );
        assert!(payload.dependencies.contains(&nested));
    }

    #[test]
    fn java_source_types_bind_from_java_package_and_reject_import_shadows() {
        let path = PathBuf::from("src/Box.java");
        let source = "package sample; import lombok.Getter; @Getter class Box { private Payload payload; } class Payload {}";
        let payload = symbol(&path, Vec::new(), "class", "Payload");
        let provider =
            java_source_provider(&path, source, &["Box", "Payload"], vec![payload.clone()]);
        let contracts = collect_java_source(&path, source, &provider);
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].jvm_signature.descriptor, "()Lsample/Payload;");
        assert!(contracts[0].dependencies.contains(&payload));

        let shadowed = "package sample; import lombok.Getter; import other.Payload; @Getter class Box { private Payload payload; } class Payload {}";
        let provider = java_source_provider(&path, shadowed, &["Box", "Payload"], vec![payload]);
        assert!(collect_java_source(&path, shadowed, &provider).is_empty());
    }

    #[test]
    fn java_source_qualified_string_stays_distinct_from_same_package_string() {
        let path = PathBuf::from("src/Box.java");
        let source = "package sample; import lombok.Getter; @Getter class Box { private java.lang.String label; private String text; } class String {}";
        let shadow = symbol(&path, Vec::new(), "class", "String");
        let provider =
            java_source_provider(&path, source, &["Box", "String"], vec![shadow.clone()]);
        let contracts = collect_java_source(&path, source, &provider);
        assert_eq!(contracts.len(), 2);
        let label = contracts
            .iter()
            .find(|contract| contract.property_name == "label")
            .unwrap();
        assert_eq!(
            label.result_type.ty,
            KotlinType::Named {
                name: "java.lang.String".into(),
                arguments: Vec::new(),
            }
        );
        let text = contracts
            .iter()
            .find(|contract| contract.property_name == "text")
            .unwrap();
        assert_eq!(
            text.result_type.ty,
            KotlinType::Named {
                name: "sample.String".into(),
                arguments: Vec::new(),
            }
        );
        assert!(text.dependencies.contains(&shadow));
    }

    #[test]
    fn java_source_getter_suppression_obeys_varargs_minimum_arity() {
        let path = PathBuf::from("src/Box.java");
        let varargs_only = "package sample; import lombok.Getter; @Getter class Box { private int count; public int getCount(String... args) { return count; } }";
        let provider = java_source_provider(&path, varargs_only, &["Box"], Vec::new());
        assert!(collect_java_source(&path, varargs_only, &provider).is_empty());

        let required = "package sample; import lombok.Getter; @Getter class Box { private int count; public int getCount(int value, String... args) { return count + value; } }";
        let provider = java_source_provider(&path, required, &["Box"], Vec::new());
        let contracts = collect_java_source(&path, required, &provider);
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].method_name, "getCount");
    }
}
