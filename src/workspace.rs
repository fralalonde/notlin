use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLanguage {
    Kotlin,
    Java,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationKind {
    Class,
    Interface,
    Enum,
    Object,
    Record,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Property,
    Method,
    Field,
    Constructor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
    pub visibility: Option<String>,
    pub is_static: bool,
    pub type_name: Option<String>,
    pub is_nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub package: Option<String>,
    pub language: SourceLanguage,
    pub kind: DeclarationKind,
    pub supertypes: Vec<String>,
    pub members: Vec<Member>,
    pub has_default_constructor_parameter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub language: SourceLanguage,
    pub package: Option<String>,
    pub imports: Vec<String>,
    pub declarations: Vec<Declaration>,
    identifier_counts: HashMap<String, usize>,
    smart_cast_properties: HashSet<String>,
}

#[derive(Debug, Default)]
pub struct SourceIndex {
    pub files: Vec<SourceFile>,
    kotlin_subtypes: HashMap<String, Vec<PathBuf>>,
}

impl SourceIndex {
    pub fn discover(root: &Path) -> Result<Self, String> {
        let mut paths = Vec::new();
        collect_sources(root, &mut paths)?;
        paths.sort();
        let total = paths.len();
        eprintln!("indexing 0/{total} sources");

        let mut files = Vec::with_capacity(total);
        for (done, path) in paths.into_iter().enumerate() {
            let path =
                fs::canonicalize(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            let language = match path.extension().and_then(|ext| ext.to_str()) {
                Some("kt") => SourceLanguage::Kotlin,
                Some("java") => SourceLanguage::Java,
                _ => continue,
            };
            let source = fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let package = package_name(&source);
            let imports = import_names(&source);
            let (declarations, identifier_counts, smart_cast_properties) =
                parse_declarations(&source, language, package.as_deref())?;
            files.push(SourceFile {
                path,
                language,
                package,
                imports,
                declarations,
                identifier_counts,
                smart_cast_properties,
            });
            let finished = done + 1;
            if finished == total || finished % 100 == 0 {
                eprintln!("indexed {finished}/{total} sources");
            }
        }
        eprintln!("resolving Kotlin subtype edges");
        let mut index = Self {
            files,
            kotlin_subtypes: HashMap::new(),
        };
        let mut subtype_edges = Vec::new();
        for file in index.kotlin_files() {
            for declaration in &file.declarations {
                for supertype in &declaration.supertypes {
                    if let Some(target) = index.resolve_type(file, supertype) {
                        subtype_edges.push((declaration_key(target), file.path.clone()));
                    }
                }
            }
        }
        for (target, path) in subtype_edges {
            index.kotlin_subtypes.entry(target).or_default().push(path);
        }
        Ok(index)
    }

    pub fn kotlin_files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files
            .iter()
            .filter(|file| file.language == SourceLanguage::Kotlin)
    }

    pub fn java_files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files
            .iter()
            .filter(|file| file.language == SourceLanguage::Java)
    }

    pub fn source_file(&self, path: &Path) -> Option<&SourceFile> {
        self.files.iter().find(|file| paths_match(&file.path, path))
    }

    pub fn has_kotlin_reference(&self, declaring_file: &Path, name: &str) -> bool {
        self.kotlin_files().any(|file| {
            let count = file.identifier_counts.get(name).copied().unwrap_or(0);
            if paths_match(&file.path, declaring_file) {
                count > 1
            } else {
                count > 0
            }
        })
    }

    pub fn declarations(&self) -> impl Iterator<Item = &Declaration> {
        self.files.iter().flat_map(|file| file.declarations.iter())
    }
    pub fn is_selected(&self, path: &Path, translation_roots: &[PathBuf]) -> bool {
        translation_roots.iter().any(|root| {
            if path.starts_with(root) {
                return true;
            }
            if root.to_string_lossy().starts_with(r"\\?\") {
                return false;
            }
            fs::canonicalize(root)
                .ok()
                .is_some_and(|canonical| path.starts_with(canonical))
        })
    }

    pub fn has_kotlin_subtype(&self, target: &Declaration) -> bool {
        self.kotlin_subtypes.contains_key(&declaration_key(target))
    }
    pub fn has_unselected_kotlin_subtype(
        &self,
        target: &Declaration,
        translation_roots: &[PathBuf],
    ) -> bool {
        self.kotlin_subtypes
            .get(&declaration_key(target))
            .is_some_and(|paths| {
                paths
                    .iter()
                    .any(|path| !self.is_selected(path, translation_roots))
            })
    }

    pub fn property_smart_cast_used_by_kotlin(
        &self,
        declaring_file: &Path,
        target: &Declaration,
    ) -> bool {
        self.kotlin_files().any(|file| {
            !paths_match(&file.path, declaring_file)
                && file
                    .identifier_counts
                    .get(&target.name)
                    .is_some_and(|count| *count > 0)
                && target.members.iter().any(|member| {
                    member.kind == MemberKind::Property
                        && file.smart_cast_properties.contains(&member.name)
                })
        })
    }

    pub fn narrows_nullable_kotlin_property(
        &self,
        source_file: &SourceFile,
        target: &Declaration,
    ) -> bool {
        target.supertypes.iter().any(|supertype| {
            let Some(contract) = self.resolve_kotlin_type(source_file, supertype) else {
                return false;
            };
            contract.members.iter().any(|contract_member| {
                contract_member.kind == MemberKind::Property
                    && contract_member.is_nullable
                    && target.members.iter().any(|member| {
                        member.kind == MemberKind::Property
                            && member.name == contract_member.name
                            && !member.is_nullable
                    })
            })
        })
    }

    fn resolve_kotlin_type<'a>(
        &'a self,
        source_file: &SourceFile,
        type_name: &str,
    ) -> Option<&'a Declaration> {
        let simple = type_name
            .trim()
            .trim_end_matches('?')
            .split('<')
            .next()
            .unwrap_or("")
            .split('(')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        let find = |qualified: &str| {
            let (package, name) = qualified.rsplit_once('.')?;
            self.declarations().find(|declaration| {
                declaration.language == SourceLanguage::Kotlin
                    && declaration.name == name
                    && declaration.package.as_deref() == Some(package)
            })
        };
        if type_name.contains('.') {
            return find(type_name.trim().trim_end_matches('?'));
        }
        for import in &source_file.imports {
            if import.ends_with(&format!(".{simple}")) {
                if let Some(found) = find(import) {
                    return Some(found);
                }
            }
            if let Some(package) = import.strip_suffix(".*") {
                if let Some(found) = find(&format!("{package}.{simple}")) {
                    return Some(found);
                }
            }
        }
        if let Some(package) = &source_file.package {
            if let Some(found) = find(&format!("{package}.{simple}")) {
                return Some(found);
            }
        }
        let mut matches = self
            .declarations()
            .filter(|declaration| declaration.language == SourceLanguage::Kotlin)
            .filter(|declaration| declaration.name == simple);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub fn resolve_type<'a>(
        &'a self,
        source_file: &SourceFile,
        type_name: &str,
    ) -> Option<&'a Declaration> {
        let simple = type_name
            .trim()
            .trim_end_matches('?')
            .split('<')
            .next()
            .unwrap_or("")
            .split('(')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        if type_name.contains('.') {
            return self.find_qualified(type_name.trim().trim_end_matches('?'));
        }
        for import in &source_file.imports {
            if import.ends_with(&format!(".{simple}")) {
                if let Some(found) = self.find_qualified(import) {
                    return Some(found);
                }
            }
            if let Some(package) = import.strip_suffix(".*") {
                if let Some(found) = self.find_qualified(&format!("{package}.{simple}")) {
                    return Some(found);
                }
            }
        }
        if let Some(package) = &source_file.package {
            if let Some(found) = self.find_qualified(&format!("{package}.{simple}")) {
                return Some(found);
            }
        }
        let mut matches = self
            .declarations()
            .filter(|declaration| declaration.name == simple);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    fn find_qualified(&self, qualified: &str) -> Option<&Declaration> {
        let (package, name) = qualified.rsplit_once('.')?;
        self.declarations().find(|declaration| {
            declaration.name == name && declaration.package.as_deref() == Some(package)
        })
    }
}

fn declaration_key(declaration: &Declaration) -> String {
    match &declaration.package {
        Some(package) => format!("{package}.{}", declaration.name),
        None => declaration.name.clone(),
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    fn normalized(path: &Path) -> String {
        let value = path.to_string_lossy().replace('\\', "/");
        let value = value.strip_prefix("//?/").unwrap_or(&value);
        if cfg!(windows) {
            value.to_ascii_lowercase()
        } else {
            value.to_string()
        }
    }
    normalized(left) == normalized(right)
}

fn collect_sources(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", root.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path, out)?;
        } else if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("kt") | Some("java")
        ) {
            out.push(path);
        }
    }
    Ok(())
}

fn package_name(source: &str) -> Option<String> {
    source.lines().map(str::trim).find_map(|line| {
        let package = line.strip_prefix("package ")?.trim();
        (!package.is_empty()).then(|| package.trim_end_matches(';').to_string())
    })
}

fn import_names(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("import "))
        .map(|import| import.trim_end_matches(';').trim().to_string())
        .filter(|import| !import.is_empty())
        .collect()
}
fn parse_declarations(
    source: &str,
    language: SourceLanguage,
    package: Option<&str>,
) -> Result<(Vec<Declaration>, HashMap<String, usize>, HashSet<String>), String> {
    let mut parser = tree_sitter::Parser::new();
    let grammar = match language {
        SourceLanguage::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        SourceLanguage::Java => tree_sitter_java::LANGUAGE.into(),
    };
    parser
        .set_language(&grammar)
        .map_err(|error| format!("failed to load source grammar: {error}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "source parse failed".to_string())?;
    let mut declarations = Vec::new();
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().children(&mut cursor) {
        let Some((kind, name_node)) = declaration_shape(node, language) else {
            continue;
        };
        let name = node_text(name_node, source)?;
        declarations.push(Declaration {
            name,
            package: package.map(str::to_string),
            language,
            kind,
            supertypes: supertypes(node, language, source),
            members: members(node, language, source),
            has_default_constructor_parameter: has_default_constructor_parameter(node, language),
        });
    }
    let mut identifier_counts = HashMap::new();
    collect_identifier_counts(tree.root_node(), source, &mut identifier_counts);
    let mut smart_cast_properties = HashSet::new();
    if language == SourceLanguage::Kotlin {
        collect_smart_cast_properties(tree.root_node(), source, &mut smart_cast_properties);
    }
    Ok((declarations, identifier_counts, smart_cast_properties))
}

fn collect_smart_cast_properties(
    node: tree_sitter::Node<'_>,
    source: &str,
    properties: &mut HashSet<String>,
) {
    if node.kind() == "is_expression" {
        if let Some(left) = node.child_by_field_name("left") {
            if left.kind() == "navigation_expression" {
                if let Some(property) = left
                    .named_children(&mut left.walk())
                    .filter(|child| child.kind() == "identifier")
                    .last()
                {
                    if let Ok(name) = property.utf8_text(source.as_bytes()) {
                        properties.insert(name.to_string());
                    }
                }
            }
        }
    }
    for child in node.named_children(&mut node.walk()) {
        collect_smart_cast_properties(child, source, properties);
    }
}

fn collect_identifier_counts(
    node: tree_sitter::Node<'_>,
    source: &str,
    counts: &mut HashMap<String, usize>,
) {
    if node.kind() == "identifier" {
        if let Ok(name) = node.utf8_text(source.as_bytes()) {
            *counts.entry(name.to_string()).or_default() += 1;
        }
    }
    for child in node.named_children(&mut node.walk()) {
        collect_identifier_counts(child, source, counts);
    }
}

fn has_default_constructor_parameter(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
) -> bool {
    if language != SourceLanguage::Kotlin {
        return false;
    }
    let Some(constructor) = node.child_by_field_name("primary_constructor").or_else(|| {
        node.children(&mut node.walk())
            .find(|child| child.kind() == "primary_constructor")
    }) else {
        return false;
    };
    let Some(parameters) = constructor
        .children(&mut constructor.walk())
        .find(|child| child.kind() == "class_parameters")
    else {
        return false;
    };
    parameters
        .named_children(&mut parameters.walk())
        .filter(|parameter| parameter.kind() == "class_parameter")
        .any(|parameter| {
            parameter
                .children(&mut parameter.walk())
                .any(|child| child.kind() == "=")
        })
}

fn declaration_shape(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
) -> Option<(DeclarationKind, tree_sitter::Node<'_>)> {
    match (language, node.kind()) {
        (SourceLanguage::Kotlin, "class_declaration") => {
            node.child_by_field_name("name").map(|name| {
                let is_interface = node
                    .children(&mut node.walk())
                    .any(|child| child.kind() == "interface");
                (
                    if is_interface {
                        DeclarationKind::Interface
                    } else {
                        DeclarationKind::Class
                    },
                    name,
                )
            })
        }
        (SourceLanguage::Kotlin, "object_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Object, name)),
        (SourceLanguage::Java, "class_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Class, name)),
        (SourceLanguage::Java, "interface_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Interface, name)),
        (SourceLanguage::Java, "enum_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Enum, name)),
        (SourceLanguage::Java, "record_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Record, name)),
        _ => None,
    }
}

fn supertypes(node: tree_sitter::Node<'_>, language: SourceLanguage, source: &str) -> Vec<String> {
    let fields: &[&str] = match language {
        SourceLanguage::Kotlin => &["delegation_specifiers"],
        SourceLanguage::Java => &["superclass", "interfaces", "super_interfaces"],
    };
    let mut nodes: Vec<tree_sitter::Node<'_>> = fields
        .iter()
        .filter_map(|field| node.child_by_field_name(field))
        .collect();
    if nodes.is_empty() {
        let kinds: &[&str] = match language {
            SourceLanguage::Kotlin => &["delegation_specifiers"],
            SourceLanguage::Java => &["superclass", "super_interfaces", "interfaces"],
        };
        nodes = node
            .children(&mut node.walk())
            .filter(|child| kinds.contains(&child.kind()))
            .collect();
    }
    nodes
        .into_iter()
        .flat_map(|child| {
            if language == SourceLanguage::Kotlin && child.kind() == "delegation_specifiers" {
                child
                    .named_children(&mut child.walk())
                    .filter(|specifier| specifier.kind() == "delegation_specifier")
                    .collect::<Vec<_>>()
            } else {
                vec![child]
            }
        })
        .map(|child| {
            child
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|text| !text.is_empty())
        .collect()
}

fn members(node: tree_sitter::Node<'_>, language: SourceLanguage, source: &str) -> Vec<Member> {
    let mut result = Vec::new();
    if language == SourceLanguage::Kotlin {
        if let Some(parameters) = node
            .children(&mut node.walk())
            .find(|child| child.kind() == "primary_constructor")
            .and_then(|constructor| {
                constructor
                    .children(&mut constructor.walk())
                    .find(|child| child.kind() == "class_parameters")
            })
        {
            for parameter in parameters
                .named_children(&mut parameters.walk())
                .filter(|parameter| parameter.kind() == "class_parameter")
            {
                let is_property = parameter
                    .children(&mut parameter.walk())
                    .any(|child| matches!(child.kind(), "val" | "var"));
                if !is_property {
                    continue;
                }
                let Some(name_node) = parameter
                    .named_children(&mut parameter.walk())
                    .find(|child| child.kind() == "identifier")
                else {
                    continue;
                };
                let type_node = parameter
                    .named_children(&mut parameter.walk())
                    .find(|child| matches!(child.kind(), "user_type" | "nullable_type"));
                result.push(Member {
                    name: node_text(name_node, source).unwrap_or_default(),
                    kind: MemberKind::Property,
                    visibility: None,
                    is_static: false,
                    type_name: type_node.and_then(|node| node_text(node, source).ok()),
                    is_nullable: type_node.is_some_and(|node| node.kind() == "nullable_type"),
                });
            }
        }
    }
    if let Some(body) = node.child_by_field_name("body").or_else(|| {
        node.children(&mut node.walk())
            .find(|child| child.kind() == "class_body")
    }) {
        result.extend(
            body.children(&mut body.walk())
                .filter_map(|child| member_from_node(child, language, source)),
        );
    }
    result
}

fn member_from_node(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
    source: &str,
) -> Option<Member> {
    let (kind, name_node) = match (language, node.kind()) {
        (SourceLanguage::Kotlin, "property_declaration") => (
            MemberKind::Property,
            node.child_by_field_name("name").or_else(|| {
                node.named_children(&mut node.walk())
                    .find(|child| child.kind() == "variable_declaration")
                    .and_then(|variable| {
                        variable
                            .named_children(&mut variable.walk())
                            .find(|child| child.kind() == "identifier")
                    })
            }),
        ),
        (SourceLanguage::Kotlin, "function_declaration") => {
            (MemberKind::Method, node.child_by_field_name("name"))
        }
        (SourceLanguage::Kotlin, "secondary_constructor") => (MemberKind::Constructor, None),
        (SourceLanguage::Java, "field_declaration") => (MemberKind::Field, first_identifier(node)),
        (SourceLanguage::Java, "method_declaration") => {
            (MemberKind::Method, node.child_by_field_name("name"))
        }
        (SourceLanguage::Java, "constructor_declaration") => {
            (MemberKind::Constructor, node.child_by_field_name("name"))
        }
        _ => return None,
    };
    let name = name_node
        .map(|node| node_text(node, source))
        .transpose()
        .ok()??;
    let modifiers = node
        .child_by_field_name("modifiers")
        .or_else(|| {
            node.children(&mut node.walk())
                .find(|child| child.kind() == "modifiers")
        })
        .map(|node| node.utf8_text(source.as_bytes()).unwrap_or(""))
        .unwrap_or("");
    let visibility = ["public", "protected", "internal", "private"]
        .iter()
        .find(|visibility| {
            modifiers
                .split_whitespace()
                .any(|word| word == **visibility)
        })
        .map(|visibility| (*visibility).to_string());
    let type_node = node
        .child_by_field_name("type")
        .or_else(|| first_type_node(node));
    let type_name = type_node.map(|node| {
        node.utf8_text(source.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string()
    });
    Some(Member {
        name: if name.is_empty() {
            "<init>".to_string()
        } else {
            name
        },
        kind,
        visibility,
        is_static: modifiers.split_whitespace().any(|word| word == "static"),
        type_name,
        is_nullable: type_node.is_some_and(|node| node.kind() == "nullable_type"),
    })
}

fn first_type_node(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if matches!(node.kind(), "nullable_type" | "user_type") {
        return Some(node);
    }
    node.named_children(&mut node.walk())
        .find_map(first_type_node)
}

fn first_identifier(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" {
            return Some(current);
        }
        stack.extend(current.children(&mut current.walk()));
    }
    None
}

fn node_text(node: tree_sitter::Node<'_>, source: &str) -> Result<String, String> {
    node.utf8_text(source.as_bytes())
        .map(str::to_string)
        .map_err(|error| format!("invalid source text: {error}"))
}
