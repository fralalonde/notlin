//! Javac attribution of immutable Java source snapshots. These facts establish
//! Java selection, not freshness of separately compiled Kotlin dependencies;
//! a workspace caller must also establish its original compilation context.
use crate::workspace::{SourceFile, SourceIndex, SourceLanguage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HELPER: &str = include_str!("java_bindings/JavaBindings.java");

#[derive(Debug)]
pub struct JavaAnalysisContext {
    pub java: PathBuf,
    pub classpath: Vec<PathBuf>,
    pub processors: Vec<String>,
    pub processor_path: Vec<PathBuf>,
    pub release: Option<u16>,
    pub enable_preview: bool,
    pub roots: Vec<String>,
    /// Configuration and source dependencies consulted by annotation processors
    /// or by the caller's original-Kotlin compilation preparation.
    pub additional_inputs: Vec<PathBuf>,
    pub timeout: Duration,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallBinding {
    pub start_byte: usize,
    pub end_byte: usize,
    pub name: String,
    pub owner_binary: String,
    pub descriptor: String,
    pub parameter_count: usize,
    pub is_static: bool,
    pub reference: bool,
    pub related_roots: Vec<String>,
}

#[derive(Debug)]
struct FileBindings {
    hash: [u8; 32],
    calls: Vec<CallBinding>,
}

#[derive(Debug)]
pub struct JavaBindings {
    files: BTreeMap<PathBuf, FileBindings>,
    classpath: ClasspathSnapshot,
    roots: BTreeSet<String>,
    names: BTreeSet<String>,
}

impl JavaBindings {
    pub fn covers_family(&self, root: &str, getter: &str) -> bool {
        self.names.contains(getter)
            && (self.roots.contains(root)
                || self
                    .files
                    .values()
                    .all(|file| file.calls.iter().all(|call| call.name != getter)))
    }
    pub fn calls(&self, file: &SourceFile) -> Result<Option<&[CallBinding]>, String> {
        let Some(bindings) = self.files.get(&file.path) else {
            return Ok(None);
        };
        if bindings.hash != *blake3::hash(file.source_text().as_bytes()).as_bytes() {
            return Err(format!(
                "Java binding source changed: {}",
                file.path.display()
            ));
        }
        Ok(Some(&bindings.calls))
    }

    pub fn verify_inputs(&self) -> Result<(), String> {
        if self.classpath != ClasspathSnapshot::capture(&self.classpath.roots)? {
            return Err("Java binding inputs changed after attribution".into());
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ClasspathSnapshot {
    roots: Vec<PathBuf>,
    files: BTreeMap<PathBuf, Option<[u8; 32]>>,
}

impl ClasspathSnapshot {
    fn capture(paths: &[PathBuf]) -> Result<Self, String> {
        fn visit(
            path: &Path,
            files: &mut BTreeMap<PathBuf, Option<[u8; 32]>>,
        ) -> Result<(), String> {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    files.insert(path.to_owned(), None);
                    return Ok(());
                }
                Err(error) => return Err(format!("{}: {error}", path.display())),
            };
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "classpath contains an untracked link: {}",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                for child in
                    fs::read_dir(path).map_err(|error| format!("{}: {error}", path.display()))?
                {
                    visit(&child.map_err(|error| error.to_string())?.path(), files)?;
                }
            } else if metadata.is_file() {
                let bytes =
                    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
                files.insert(path.to_owned(), Some(*blake3::hash(&bytes).as_bytes()));
            } else {
                return Err(format!("unsupported classpath entry: {}", path.display()));
            }
            Ok(())
        }
        let mut roots = Vec::new();
        let mut files = BTreeMap::new();
        for path in paths {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()
                    .map_err(|error| error.to_string())?
                    .join(path)
            };
            let path = fs::canonicalize(&path).unwrap_or(path);
            visit(&path, &mut files)?;
            roots.push(path);
        }
        Ok(Self { roots, files })
    }
}

#[derive(Serialize)]
struct Input<'a> {
    id: usize,
    path: String,
    hash: String,
    source: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    token: String,
    success: bool,
    files: Vec<BoundFile>,
    errors: Vec<serde_json::Value>,
    roots: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundFile {
    id: usize,
    hash: String,
    calls: Vec<CallBinding>,
}

// JavaFileObject needs an absolute filesystem URI, not a human display path.
fn java_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\UNC\")
        .map(|tail| format!(r"\\{tail}"))
        .unwrap_or_else(|| text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned())
}

/// The stage must be new. Source content crosses the process boundary as
/// strings; javac cannot silently reread a newer version of those files.
pub fn collect(
    index: &SourceIndex,
    paths: &[PathBuf],
    names: &[String],
    context: &JavaAnalysisContext,
    stage: &Path,
) -> Result<JavaBindings, String> {
    let files = paths
        .iter()
        .map(|path| {
            index
                .source_file(path)
                .filter(|file| file.language == SourceLanguage::Java)
                .ok_or_else(|| format!("Java analysis source is not indexed: {}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if files
        .iter()
        .map(|file| &file.path)
        .collect::<BTreeSet<_>>()
        .len()
        != files.len()
    {
        return Err("duplicate Java analysis source".into());
    }
    let mut all_classpath = context.classpath.clone();
    all_classpath.extend(context.processor_path.iter().cloned());
    all_classpath.extend(context.additional_inputs.iter().cloned());
    let snapshot = ClasspathSnapshot::capture(&all_classpath)?;
    let classpath = &snapshot.roots[..context.classpath.len()];
    let processor_path = &snapshot.roots
        [context.classpath.len()..context.classpath.len() + context.processor_path.len()];
    let inputs = files
        .iter()
        .enumerate()
        .map(|(id, file)| Input {
            id,
            path: java_path(&file.path),
            hash: blake3::hash(file.source_text().as_bytes())
                .to_hex()
                .to_string(),
            source: file.source_text(),
        })
        .collect::<Vec<_>>();
    let mut options = Vec::new();
    if let Some(release) = context.release {
        options.extend(["--release".to_owned(), release.to_string()]);
    }
    if context.enable_preview {
        options.push("--enable-preview".into());
    }
    let mut request = serde_json::json!({
        "version":1,"sources":inputs,"names":names,"roots":context.roots,
        "classpath":classpath.iter().map(|path|java_path(path)).collect::<Vec<_>>(),
        "processor_path":processor_path.iter().map(|path|java_path(path)).collect::<Vec<_>>(),
        "processors":context.processors,"compiler_options":options,
    });
    let token = blake3::hash(
        &serde_json::to_vec(&(&request, HELPER, &snapshot.files, &context.java))
            .map_err(|error| error.to_string())?,
    )
    .to_hex()
    .to_string();
    request["token"] = token.clone().into();
    fs::create_dir(stage)
        .map_err(|error| format!("new Java analysis stage {}: {error}", stage.display()))?;
    let stage = fs::canonicalize(stage).map_err(|error| error.to_string())?;
    let helper = stage.join("JavaBindings.java");
    let input = stage.join("request.json");
    let output = stage.join("response.json");
    fs::write(&helper, HELPER).map_err(|error| error.to_string())?;
    fs::write(
        &input,
        serde_json::to_vec(&request).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let stdout = fs::File::create(stage.join("stdout.log")).map_err(|error| error.to_string())?;
    let stderr = fs::File::create(stage.join("stderr.log")).map_err(|error| error.to_string())?;
    let mut child = Command::new(&context.java)
        .arg(java_path(&helper))
        .arg(java_path(&input))
        .arg(java_path(&output))
        .current_dir(&stage)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|error| format!("Java attribution: {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < context.timeout => {
                std::thread::sleep(Duration::from_millis(25))
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => error.to_string(),
                    _ => "Java attribution timed out".into(),
                });
            }
        }
    };
    if !status.success() {
        return Err(format!(
            "Java attribution failed ({status}); diagnostics: {}",
            stage.join("stderr.log").display()
        ));
    }
    let response: Response =
        serde_json::from_slice(&fs::read(&output).map_err(|error| error.to_string())?)
            .map_err(|error| format!("Java attribution response: {error}"))?;
    if response.version != 1 || response.token != token {
        return Err("Java attribution response belongs to a different request".into());
    }
    if !response.success || !response.errors.is_empty() {
        return Err(format!(
            "original Java sources did not attribute: {}",
            serde_json::to_string(&response.errors).map_err(|error| error.to_string())?
        ));
    }
    if response.files.len() != inputs.len() {
        return Err("incomplete Java attribution batch".into());
    }
    let found_roots = response.roots.iter().cloned().collect::<BTreeSet<_>>();
    if found_roots.len() != response.roots.len()
        || found_roots.iter().any(|root| !context.roots.contains(root))
    {
        return Err("invalid Java attribution hierarchy roots".into());
    }
    let mut bound = BTreeMap::new();
    for file in response.files {
        let original = files
            .get(file.id)
            .ok_or("unknown Java attribution source")?;
        if file.hash != inputs[file.id].hash {
            return Err("Java attribution source receipt mismatch".into());
        }
        let mut spans = BTreeSet::new();
        for call in &file.calls {
            if call.start_byte >= call.end_byte
                || original
                    .source_text()
                    .get(call.start_byte..call.end_byte)
                    .is_none()
                || !names.contains(&call.name)
                || call.owner_binary.is_empty()
                || !call.descriptor.starts_with('(')
                || !spans.insert((call.start_byte, call.end_byte))
            {
                return Err("invalid or duplicate Java call binding".into());
            }
        }
        if bound
            .insert(
                original.path.clone(),
                FileBindings {
                    hash: *blake3::hash(original.source_text().as_bytes()).as_bytes(),
                    calls: file.calls,
                },
            )
            .is_some()
        {
            return Err("duplicate Java attribution file".into());
        }
    }
    let bindings = JavaBindings {
        files: bound,
        classpath: snapshot,
        roots: found_roots,
        names: names.iter().cloned().collect(),
    };
    bindings.verify_inputs()?;
    Ok(bindings)
}
