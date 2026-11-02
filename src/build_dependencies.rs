//! Build dependencies required by accepted generated sources. Planning is pure
//! with respect to writes; all build snapshots are checked before application.

use crate::cli::Cli;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const PACKAGE: &str = "org.jspecify.annotations";
const MARKER: &str = "// NOTLIN: JSpecify dependency for generated Java";

#[derive(Default)]
pub struct BuildDependencyPlan {
    edits: Vec<BuildEdit>,
}

struct BuildEdit {
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl BuildDependencyPlan {
    pub fn apply(&self) -> Result<(), String> {
        // Verify the entire batch before writing any build script.
        for edit in &self.edits {
            let current = std::fs::read(&edit.path)
                .map_err(|error| format!("{}: {error}", crate::paths::display(&edit.path)))?;
            if current != edit.before {
                return Err(format!(
                    "Gradle script changed after dependency planning: {}",
                    crate::paths::display(&edit.path)
                ));
            }
        }
        for edit in &self.edits {
            std::fs::write(&edit.path, &edit.after)
                .map_err(|error| format!("{}: {error}", crate::paths::display(&edit.path)))?;
            log::debug!(
                "configured JSpecify in {}",
                crate::paths::display(&edit.path)
            );
        }
        Ok(())
    }
}

pub fn plan_jspecify_dependencies<'a>(
    cli: &Cli,
    sources: impl IntoIterator<Item = (&'a Path, &'a str)>,
    workspace_root: &Path,
) -> Result<BuildDependencyPlan, String> {
    if !cli.migrates_in_place() || cli.out_dir.is_some() {
        return Ok(BuildDependencyPlan::default());
    }
    let root = std::fs::canonicalize(workspace_root)
        .map_err(|error| format!("{}: {error}", crate::paths::display(workspace_root)))?;
    let mut locations = HashMap::new();
    let mut required = BTreeSet::new();
    let mut origins = BTreeMap::<_, Vec<_>>::new();
    for (path, source) in sources {
        if source.contains("jspecify")
            && let Some(directory) = path.parent()
        {
            let directory = if directory.as_os_str().is_empty() {
                Path::new(".")
            } else {
                directory
            };
            origins.entry(directory).or_default().push(source);
        }
    }
    for (directory, sources) in origins {
        let directory = std::fs::canonicalize(directory)
            .map_err(|error| format!("{}: {error}", crate::paths::display(directory)))?;
        let Some(build) = nearest_gradle_script(&directory, &root, &mut locations) else {
            continue;
        };
        if !required.contains(&build) && sources.into_iter().any(uses_jspecify) {
            required.insert(build);
        }
    }
    let mut edits = Vec::new();
    for path in required {
        let before = std::fs::read(&path)
            .map_err(|error| format!("{}: {error}", crate::paths::display(&path)))?;
        let source = std::str::from_utf8(&before)
            .map_err(|error| format!("{}: {error}", crate::paths::display(&path)))?;
        if source.lines().any(|line| line.trim() == MARKER) {
            continue;
        }
        let kotlin = path.extension().is_some_and(|extension| extension == "kts");
        let newline = if source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut after = source.to_owned();
        if !after.is_empty() && !after.ends_with('\n') {
            after.push_str(newline);
        }
        after.push_str(newline);
        after.push_str(&gradle_dependency_block(kotlin).replace('\n', newline));
        edits.push(BuildEdit {
            path,
            before,
            after: after.into_bytes(),
        });
    }
    Ok(BuildDependencyPlan { edits })
}

fn nearest_gradle_script(
    directory: &Path,
    root: &Path,
    cache: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> Option<PathBuf> {
    let mut visited = Vec::new();
    let mut current = directory;
    let result = loop {
        if !current.starts_with(root) {
            break None;
        }
        if let Some(found) = cache.get(current) {
            break found.clone();
        }
        visited.push(current.to_path_buf());
        if let Some(script) = ["build.gradle", "build.gradle.kts"]
            .into_iter()
            .map(|name| current.join(name))
            .find(|path| path.is_file())
        {
            // Do not modify a shared build script outside the selected workspace.
            break std::fs::canonicalize(script)
                .ok()
                .filter(|path| path.starts_with(root));
        }
        if current == root {
            break None;
        }
        let Some(parent) = current.parent() else {
            break None;
        };
        current = parent;
    };
    for directory in visited {
        cache.insert(directory, result.clone());
    }
    result
}

fn uses_jspecify(source: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("Java grammar");
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        let name = match node.kind() {
            "import_declaration" => node
                .named_children(&mut node.walk())
                .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier")),
            "annotation" | "marker_annotation" => node.child_by_field_name("name"),
            "scoped_type_identifier" => Some(node),
            _ => None,
        };
        if name
            .and_then(|name| qualified_name(name, source))
            .is_some_and(|name| {
                name.starts_with(PACKAGE) && name.as_bytes().get(PACKAGE.len()) == Some(&b'.')
            })
        {
            return true;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    false
}

fn qualified_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let mut pending = vec![node];
    let mut names = Vec::new();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" | "type_identifier" => names.push(node.utf8_text(source.as_bytes()).ok()?),
            "scoped_identifier" | "scoped_type_identifier" => {
                let children = node
                    .named_children(&mut node.walk())
                    .filter(|child| !child.is_extra())
                    .collect::<Vec<_>>();
                pending.extend(children.into_iter().rev());
            }
            _ => return None,
        }
    }
    Some(names.join("."))
}

fn gradle_dependency_block(kotlin: bool) -> String {
    // JSpecify recommends api for libraries and implementation otherwise,
    // including the annotations at runtime: https://jspecify.dev/docs/using/
    let binding = if kotlin { "val" } else { "def" };
    format!(
        "{MARKER}\nallprojects {{\n    plugins.withId(\"java\") {{\n        {binding} notlinJspecifyConfiguration = configurations.findByName(\"api\") ?: configurations.getByName(\"implementation\")\n        if (!notlinJspecifyConfiguration.allDependencies.any {{ it.group == \"org.jspecify\" && it.name == \"jspecify\" }}) {{\n            dependencies.add(notlinJspecifyConfiguration.name, \"org.jspecify:jspecify:1.0.0\")\n        }}\n    }}\n}}\n"
    )
}
