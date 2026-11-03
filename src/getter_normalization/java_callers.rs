//! A source-bound ABI table plus javac's selected declaration replaces receiver
//! inference for attributed Java. Argument overloads are selected by javac too.
use super::GetterFamily;
use crate::java_bindings::JavaBindings;
use crate::semantics::SourceLocation;
use crate::semantics::{self, SymbolId};
use crate::translation_plan::PlannedSourceEdit;
use crate::workspace::{Declaration, MemberKind, SourceFile, SourceIndex};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

fn binary(symbol: &SymbolId) -> String {
    let mut names = symbol.owner_path.clone();
    names.push(symbol.name.clone());
    format!(
        "{}{}{}",
        symbol.package,
        if symbol.package.is_empty() { "" } else { "." },
        names.join("$")
    )
}
fn canonical(symbol: &SymbolId) -> String {
    let mut names = symbol.owner_path.clone();
    names.push(symbol.name.clone());
    format!(
        "{}{}{}",
        symbol.package,
        if symbol.package.is_empty() { "" } else { "." },
        names.join(".")
    )
}
fn declaration<'a>(index: &'a SourceIndex, symbol: &SymbolId) -> Option<&'a Declaration> {
    index
        .source_file(&symbol.file)?
        .declarations
        .iter()
        .find(|owner| semantics::workspace_symbol(index, owner) == *symbol)
}
fn getter_descriptor(
    index: &SourceIndex,
    file: &SourceFile,
    owner: &Declaration,
    raw: &str,
    method_parameters: &[String],
) -> Option<String> {
    let core = raw.trim().trim_end_matches('?');
    if method_parameters
        .iter()
        .chain(&owner.type_params)
        .any(|name| name == core)
    {
        return None;
    }
    let owner_symbol = semantics::workspace_symbol(index, owner);
    for ancestor in &file.declarations {
        let symbol = semantics::workspace_symbol(index, ancestor);
        let mut scope = symbol.owner_path.clone();
        scope.push(symbol.name);
        if owner_symbol.owner_path.starts_with(&scope)
            && ancestor.type_params.iter().any(|name| name == core)
        {
            return None;
        }
    }
    if file.language == crate::workspace::SourceLanguage::Java
        && let Some(primitive) = crate::jvm_primitive::JvmPrimitive::java(core)
    {
        return Some(primitive.getter_descriptor.into());
    }
    if let Some(value) = index.builtin_getter_descriptor_in_file(&file.path, core) {
        if raw.trim().ends_with('?')
            && let Some(primitive) =
                crate::jvm_primitive::JvmPrimitive::kotlin(core.rsplit('.').next()?)
        {
            return Some(format!(
                "()L{};",
                primitive.boxed_qualified.replace('.', "/")
            ));
        }
        return Some(value.into());
    }
    let parsed = semantics::TypeRef::parse(raw);
    let name = match parsed.ty {
        semantics::KotlinType::Named { name, .. } | semantics::KotlinType::Primitive(name) => name,
        _ => return None,
    };
    let binding = crate::workspace::property_receiver_type_binding(index, file, &name)?;
    let value = if let Some(class) = binding.declaration {
        if class.is_jvm_value_class {
            return None;
        }
        format!(
            "L{};",
            binary(&semantics::workspace_symbol(index, class)).replace('.', "/")
        )
    } else if let Some(primitive) = binding
        .qualified
        .strip_prefix("kotlin.")
        .and_then(crate::jvm_primitive::JvmPrimitive::kotlin)
    {
        if parsed.nullable {
            format!("L{};", primitive.boxed_qualified.replace('.', "/"))
        } else {
            primitive.descriptor.into()
        }
    } else {
        let mapped = match binding.qualified.as_str() {
            "kotlin.Any" => "java.lang.Object",
            "kotlin.String" => "java.lang.String",
            "kotlin.CharSequence" => "java.lang.CharSequence",
            "kotlin.collections.List" | "kotlin.collections.MutableList" => "java.util.List",
            "kotlin.collections.Set" | "kotlin.collections.MutableSet" => "java.util.Set",
            "kotlin.collections.Map" | "kotlin.collections.MutableMap" => "java.util.Map",
            "kotlin.collections.Collection" | "kotlin.collections.MutableCollection" => {
                "java.util.Collection"
            }
            "kotlin.collections.Iterable" => "java.lang.Iterable",
            qualified if qualified.starts_with("java.lang.") && !qualified[10..].contains('.') => {
                qualified
            }
            _ => return None,
        };
        format!("L{};", mapped.replace('.', "/"))
    };
    Some(format!("(){value}"))
}

/// A complete set of original Java attribution contexts is required. Missing
/// files, mismatched sources and unknown related declarations never authorize a
/// partial family migration. Compiled-Kotlin freshness is an additional caller
/// obligation; this API does not infer it from the presence of class files.
pub fn plan_java_callers(
    index: &SourceIndex,
    family: &GetterFamily,
    contexts: &[JavaBindings],
) -> Result<JavaCallerPlan, String> {
    let root = canonical(&family.root);
    for context in contexts {
        if !context.covers_family(&root, &family.original_getter) {
            return Err("Java attribution did not cover this getter family".into());
        }
        context.verify_inputs()?;
    }
    // 0 is a property ABI; 1 is an independent source function. Conflicting
    // source roles remain unknown even if the compiler selected that owner.
    let mut roles = BTreeMap::<(String, String), BTreeSet<u8>>::new();
    for property in &family.property_owners {
        let file = index
            .source_file(&property.owner.file)
            .ok_or("property source missing")?;
        let owner = declaration(index, &property.owner).ok_or("property owner missing")?;
        if let Some(descriptor) = property
            .declared_type
            .as_deref()
            .and_then(|raw| getter_descriptor(index, file, owner, raw, &[]))
        {
            roles
                .entry((binary(&property.owner), descriptor))
                .or_default()
                .insert(0);
        }
    }
    for function in family
        .independent_methods
        .iter()
        .chain(&family.java_implementations)
    {
        let file = index
            .source_file(&function.file)
            .ok_or("getter source missing")?;
        let owner = file
            .declarations
            .iter()
            .find(|owner| {
                let symbol = semantics::workspace_symbol(index, owner);
                let mut path = symbol.owner_path;
                path.push(symbol.name);
                path == function.owner_path
            })
            .ok_or("getter owner missing")?;
        for member in owner.members.iter().filter(|member| {
            member.kind == MemberKind::Method
                && member.name == function.name
                && member.parameter_types.is_empty()
                && !member.is_static
        }) {
            if let Some(descriptor) = member
                .type_name
                .as_deref()
                .and_then(|raw| getter_descriptor(index, file, owner, raw, &member.type_parameters))
            {
                let role = u8::from(!family.java_implementations.contains(function));
                roles
                    .entry((
                        binary(&semantics::workspace_symbol(index, owner)),
                        descriptor,
                    ))
                    .or_default()
                    .insert(role);
            }
        }
    }
    let mut plan = JavaCallerPlan::default();
    for file in index.java_files() {
        let mut calls = None;
        for context in contexts {
            if let Some(candidate) = context.calls(file)? {
                if calls.is_some() {
                    return Err(format!(
                        "multiple attribution contexts for {}",
                        file.path.display()
                    ));
                }
                calls = Some(candidate);
            }
        }
        let calls = calls.ok_or_else(|| {
            format!(
                "Java source has no original attribution context: {}",
                file.path.display()
            )
        })?;
        for call in calls
            .iter()
            .filter(|call| call.name == family.original_getter)
        {
            let location = semantics::SourceLocation {
                file: file.path.clone(),
                snapshot_hash: *blake3::hash(file.source_text().as_bytes()).as_bytes(),
                start_byte: call.start_byte,
                end_byte: call.end_byte,
            };
            if call.is_static || call.parameter_count != 0 || !call.related_roots.contains(&root) {
                plan.unrelated_calls.push(location);
                continue;
            }
            match roles
                .get(&(call.owner_binary.clone(), call.descriptor.clone()))
                .map(|roles| roles.iter().copied().collect::<Vec<_>>())
                .as_deref()
            {
                Some([0]) => plan.edits.push(PlannedSourceEdit {
                    location,
                    replacement: family.proposed_getter.clone(),
                    speculative: true,
                }),
                Some([1]) => plan.independent_calls.push(location),
                _ => plan.unresolved_calls.push(location),
            }
        }
    }
    plan.edits.sort_by(|a, b| {
        (&a.location.file, a.location.start_byte).cmp(&(&b.location.file, b.location.start_byte))
    });
    Ok(plan)
}

#[derive(Debug, Default, Serialize)]
pub struct JavaCallerPlan {
    pub edits: Vec<PlannedSourceEdit>,
    pub independent_calls: Vec<SourceLocation>,
    pub unrelated_calls: Vec<SourceLocation>,
    pub unresolved_calls: Vec<SourceLocation>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn fixture() -> SourceIndex {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "getter-abi-types-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("Types.kt"),"package sample\nclass Shadow<String>(val id:String)\nclass Plain(val id:Int?)\nclass Outer<String> {inner class Nested(val id:String)}\n").unwrap();
        std::fs::write(
            root.join("Keyword.java"),
            "package sample; class Keyword {int getId(){return 1;}}\n",
        )
        .unwrap();
        SourceIndex::discover(&root).unwrap()
    }

    #[test]
    fn lexical_parameters_do_not_borrow_builtin_jvm_descriptors() {
        let index = fixture();
        for name in ["Shadow", "Nested"] {
            let owner = index.declarations_named(name).next().unwrap();
            let file = index.declaration_source_file(owner).unwrap();
            assert_eq!(getter_descriptor(&index, file, owner, "String", &[]), None);
            assert_eq!(
                getter_descriptor(&index, file, owner, "kotlin.String", &[]),
                Some("()Ljava/lang/String;".into())
            );
        }
        let owner = index.declarations_named("Plain").next().unwrap();
        let file = index.declaration_source_file(owner).unwrap();
        assert_eq!(
            getter_descriptor(&index, file, owner, "String", &["String".into()]),
            None
        );
    }

    #[test]
    fn nullable_kotlin_primitives_and_java_keywords_have_distinct_jvm_shapes() {
        let index = fixture();
        let owner = index.declarations_named("Plain").next().unwrap();
        let file = index.declaration_source_file(owner).unwrap();
        assert_eq!(
            getter_descriptor(&index, file, owner, "Int?", &[]),
            Some("()Ljava/lang/Integer;".into())
        );
        assert_eq!(
            getter_descriptor(&index, file, owner, "Int", &[]),
            Some("()I".into())
        );
        let owner = index.declarations_named("Keyword").next().unwrap();
        let file = index.declaration_source_file(owner).unwrap();
        assert_eq!(
            getter_descriptor(&index, file, owner, "int", &[]),
            Some("()I".into())
        );
    }
}
