//! Declared Gradle Kotlin/JVM defaults, without executing build scripts.
//! Dynamic compiler arguments and mixed/legacy configurations stay unproved.
//! Kotlin language versions 2.2 onward default to JVM interface methods:
//! https://kotlinlang.org/docs/whatsnew22.html#changes-to-default-method-generation-for-interface-functions

use super::SourceFile;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub(super) struct Configuration {
    pub modern: bool,
    pub inputs: Vec<(PathBuf, Option<Vec<u8>>)>,
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Symbol(u8),
}

fn tokens(source: &str) -> Option<Vec<Token>> {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut result = Vec::new();
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            index += 2;
            let mut depth = 1;
            while index < bytes.len() && depth > 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            if depth != 0 {
                return None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            let width = if bytes[index..].starts_with(&[byte; 3]) {
                3
            } else {
                1
            };
            index += width;
            let start = index;
            let delimiter = [byte; 3];
            while index < bytes.len() && !bytes[index..].starts_with(&delimiter[..width]) {
                if bytes[index] == b'\\' {
                    index += 1;
                }
                index += 1;
            }
            if index >= bytes.len() {
                return None;
            }
            result.push(Token::Quoted(source.get(start..index)?.to_owned()));
            index += width;
        } else if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
            {
                index += 1;
            }
            result.push(Token::Word(source.get(start..index)?.to_owned()));
        } else if byte == b'/' {
            // Groovy slashy strings and arbitrary arithmetic need a fuller
            // build-language evaluator; neither can establish this fact.
            return None;
        } else {
            result.push(Token::Symbol(byte));
            index += 1;
        }
    }
    Some(result)
}

fn word(token: Option<&Token>, wanted: &str) -> bool {
    matches!(token, Some(Token::Word(value)) if value == wanted)
}

fn version_is_modern(version: &str) -> bool {
    let parts = version
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>();
    matches!(parts.as_deref(), Ok([2, 2..=4, ..]))
}

fn literal_mode(tokens: &[Token], index: usize) -> Option<bool> {
    let mut cursor = index + 1;
    if tokens.get(cursor) == Some(&Token::Symbol(b'=')) {
        cursor += 1;
    } else if tokens.get(cursor) == Some(&Token::Symbol(b'.'))
        && (word(tokens.get(cursor + 1), "set") || word(tokens.get(cursor + 1), "convention"))
        && tokens.get(cursor + 2) == Some(&Token::Symbol(b'('))
    {
        cursor += 3;
    } else {
        return None;
    }
    let mut names = Vec::new();
    loop {
        let Some(Token::Word(name)) = tokens.get(cursor) else {
            return None;
        };
        names.push(name.as_str());
        cursor += 1;
        if tokens.get(cursor) != Some(&Token::Symbol(b'.')) {
            break;
        }
        cursor += 1;
    }
    let (mode, owner) = names.split_last()?;
    if owner != ["JvmDefaultMode"]
        && owner
            != [
                "org",
                "jetbrains",
                "kotlin",
                "gradle",
                "dsl",
                "JvmDefaultMode",
            ]
    {
        return None;
    }
    match *mode {
        "ENABLE" | "NO_COMPATIBILITY" => Some(true),
        "DISABLE" => Some(false),
        _ => None,
    }
}

fn declared_mode(scripts: &[String], properties: &BTreeMap<String, Option<String>>) -> bool {
    let mut versions = Vec::new();
    for script in scripts {
        let Some(tokens) = tokens(script) else {
            return false;
        };
        let mut blocks = Vec::new();
        for (index, token) in tokens.iter().enumerate() {
            if token == &Token::Symbol(b'{') {
                blocks.push(index > 0 && word(tokens.get(index - 1), "plugins"));
            }
            if token == &Token::Symbol(b'}') {
                blocks.pop();
            }
            if word(Some(token), "freeCompilerArgs") {
                return false;
            }
            if word(Some(token), "apply")
                && (word(tokens.get(index + 1), "from") || word(tokens.get(index + 2), "from"))
            {
                return false;
            }
            if word(Some(token), "languageVersion")
                && !tokens[index + 1..]
                    .iter()
                    .take(7)
                    .any(|token| word(Some(token), "JavaLanguageVersion"))
            {
                return false;
            }
            if word(Some(token), "jvmDefault") && literal_mode(&tokens, index) != Some(true) {
                return false;
            }
            if !word(Some(token), "id") && !word(Some(token), "kotlin") {
                continue;
            }
            if !blocks.contains(&true) {
                continue;
            }
            let mut cursor = index + 1;
            let parenthesized = tokens.get(cursor) == Some(&Token::Symbol(b'('));
            if parenthesized {
                cursor += 1;
            }
            let Some(Token::Quoted(plugin)) = tokens.get(cursor) else {
                continue;
            };
            let jvm = plugin == "org.jetbrains.kotlin.jvm"
                || word(Some(token), "kotlin") && plugin == "jvm";
            if !jvm {
                continue;
            }
            cursor += 1;
            if parenthesized {
                if tokens.get(cursor) != Some(&Token::Symbol(b')')) {
                    return false;
                }
                cursor += 1;
            }
            if !word(tokens.get(cursor), "version") {
                continue;
            }
            let Some(Token::Quoted(version)) = tokens.get(cursor + 1) else {
                return false;
            };
            let version = if let Some(name) = version
                .strip_prefix("${")
                .and_then(|name| name.strip_suffix('}'))
            {
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return false;
                }
                let Some(Some(version)) = properties.get(name) else {
                    return false;
                };
                version
            } else {
                version
            };
            versions.push(version_is_modern(version));
        }
    }
    !versions.is_empty() && versions.iter().all(|modern| *modern)
}

pub(super) fn discover(root: &Path, files: &[SourceFile]) -> Configuration {
    let mut directories = BTreeSet::from([root.to_path_buf()]);
    for file in files {
        if let Some(module) = file
            .path
            .ancestors()
            .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "src"))
            .and_then(Path::parent)
            && module.starts_with(root)
        {
            directories.insert(module.to_path_buf());
        }
    }
    let mut inputs = Vec::new();
    let mut scripts = Vec::new();
    let mut properties = BTreeMap::<String, Option<String>>::new();
    for directory in directories {
        for name in [
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "gradle.properties",
        ] {
            let path = directory.join(name);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return Configuration::default(),
            };
            if let Some(bytes) = &bytes {
                let Ok(source) = std::str::from_utf8(bytes) else {
                    return Configuration::default();
                };
                if name == "gradle.properties" {
                    for line in source
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.starts_with(['#', '!']))
                    {
                        if let Some((key, value)) = line.split_once('=') {
                            let value = value.trim().to_owned();
                            properties
                                .entry(key.trim().to_owned())
                                .and_modify(|previous| {
                                    if previous.as_ref() != Some(&value) {
                                        *previous = None;
                                    }
                                })
                                .or_insert(Some(value));
                        }
                    }
                } else {
                    scripts.push(source.to_owned());
                }
            }
            inputs.push((path, bytes));
        }
    }
    let modern = !root.join("buildSrc").exists() && declared_mode(&scripts, &properties);
    Configuration { modern, inputs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    fn modern(script: &str) -> bool {
        declared_mode(&[script.to_owned()], &BTreeMap::new())
    }
    #[test]
    fn literal_plugins_and_modes_are_distinct_from_comments_and_dynamic_settings() {
        assert!(modern("plugins { kotlin(\"jvm\") version \"2.3.21\" }"));
        assert!(modern(
            "plugins { id 'org.jetbrains.kotlin.jvm' version '2.2.0' } kotlin { compilerOptions { jvmDefault = JvmDefaultMode.NO_COMPATIBILITY } }"
        ));
        assert!(!modern("plugins { kotlin(\"jvm\") version \"2.1.21\" }"));
        assert!(!modern("// plugins { kotlin(\"jvm\") version \"2.3.21\" }"));
        assert!(!modern(
            "plugins { kotlin(\"jvm\") version \"2.3.21\" } compilerOptions { jvmDefault = JvmDefaultMode.DISABLE }"
        ));
        assert!(!modern(
            "plugins { kotlin(\"jvm\") version \"2.3.21\" } compilerOptions { freeCompilerArgs.addAll(extraArguments) }"
        ));
        assert!(!modern(
            "plugins { kotlin(\"jvm\") version \"2.3.21\" } compilerOptions { languageVersion.set(KotlinVersion.KOTLIN_2_1) }"
        ));
        assert!(modern(
            "plugins { kotlin(\"jvm\") version \"2.3.21\" } java { toolchain { languageVersion.set(JavaLanguageVersion.of(25)) } }"
        ));
    }
    #[test]
    fn interpolation_uses_one_unambiguous_property_value() {
        let script = vec![
            "plugins { id 'org.jetbrains.kotlin.jvm' version \"${kotlinVersion}\" }".to_owned(),
        ];
        assert!(declared_mode(
            &script,
            &BTreeMap::from([("kotlinVersion".into(), Some("2.3.21".into()))])
        ));
        assert!(!declared_mode(
            &script,
            &BTreeMap::from([("kotlinVersion".into(), None)])
        ));
    }

    #[test]
    fn compiler_settings_survive_overlays_and_gate_the_actual_write_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "notlin-jvm-default-snapshot-{}",
            std::process::id()
        ));
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _scratch = Scratch(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let build = "plugins { kotlin(\"jvm\") version \"2.3.21\" }\n";
        let build_path = root.join("build.gradle.kts");
        std::fs::write(&build_path, build).unwrap();
        let source = "package sample\ninterface Parent {\n    val value: String\n    suspend fun retained()\n}\ninterface Marker : Parent\nclass Child(override val value: String): Marker {\n    override suspend fun retained() {}\n}\n";
        let path = root.join("Types.kt");
        std::fs::write(&path, source).unwrap();
        let index = super::super::SourceIndex::discover(&root).unwrap();
        assert!(index.has_modern_kotlin_interface_defaults());
        assert!(
            index
                .with_overlays(&[])
                .unwrap()
                .has_modern_kotlin_interface_defaults()
        );
        let cli = crate::cli::Cli::parse_from([
            "notlin",
            "--jvm-facts=off",
            "--in-place",
            "--root",
            root.to_str().unwrap(),
            path.to_str().unwrap(),
        ]);
        let plan = crate::migration_pipeline::plan_workspace_migration(
            vec![(path, source.to_owned())],
            &cli,
            &index,
            std::slice::from_ref(&root),
            |_| {},
        )
        .unwrap();
        assert!(
            plan.generated_java
                .iter()
                .any(|output| output.name == "Marker.java"),
            "outputs={:?}; decisions={:?}",
            plan.generated_java
                .iter()
                .map(|output| &output.name)
                .collect::<Vec<_>>(),
            plan.translation_plans,
        );
        crate::migration_pipeline::verify_original_snapshots(&plan).unwrap();
        std::fs::write(
            &build_path,
            format!(
                "{build}\nkotlin {{ compilerOptions {{ jvmDefault = JvmDefaultMode.DISABLE }} }}\n"
            ),
        )
        .unwrap();
        assert!(crate::migration_pipeline::verify_original_snapshots(&plan).is_err());
        assert!(
            !super::super::SourceIndex::discover(&root)
                .unwrap()
                .has_modern_kotlin_interface_defaults()
        );
        std::fs::write(&build_path, build).unwrap();
        std::fs::write(
            root.join("build.gradle"),
            "kotlin { compilerOptions { jvmDefault = JvmDefaultMode.DISABLE } }\n",
        )
        .unwrap();
        assert!(crate::migration_pipeline::verify_original_snapshots(&plan).is_err());
    }
}
