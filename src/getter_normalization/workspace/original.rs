//! Build-system preparation of original classes, before any virtual edits.
use super::*;
use serde::Deserialize;
use std::{
    fs,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

type Snapshots = Vec<(PathBuf, Option<Vec<u8>>)>;

pub(super) struct Original {
    pub stage: PathBuf,
    pub contexts: Vec<(Vec<PathBuf>, JavaAnalysisContext)>,
    pub snapshots: Vec<(PathBuf, Option<Vec<u8>>)>,
    pub analysis_index: Option<SourceIndex>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    sources: Vec<PathBuf>,
    classpath: Vec<PathBuf>,
    processor_path: Vec<PathBuf>,
    java: PathBuf,
    release: u16,
    enable_preview: bool,
}

fn snapshots(index: &SourceIndex, root: &Path) -> Result<Snapshots, String> {
    let mut inputs = index.kotlin_compiler_configuration_snapshots();
    for file in index.kotlin_files().chain(index.java_files()) {
        inputs.push((
            file.path.clone(),
            Some(file.source_text().as_bytes().to_vec()),
        ));
        for parent in file.path.ancestors().skip(1) {
            let path = parent.join("lombok.config");
            let contents = match fs::read(&path) {
                Ok(contents) => Some(contents),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.to_string()),
            };
            inputs.push((path, contents));
        }
    }
    // Build scripts may apply shared configuration, version catalogs, wrapper
    // properties, or buildSrc plugins outside the source index.
    fn visit(path: &Path, inputs: &mut Vec<(PathBuf, Option<Vec<u8>>)>) -> Result<(), String> {
        for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            if kind.is_dir() {
                if ![
                    ".git",
                    ".gradle",
                    ".notlin",
                    ".idea",
                    "target",
                    "build",
                    "node_modules",
                ]
                .contains(&name.to_string_lossy().as_ref())
                {
                    visit(&path, inputs)?;
                }
            } else if name == "lombok.config"
                || ["gradle", "kts", "kt", "java", "toml", "properties"].contains(
                    &path
                        .extension()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .as_ref(),
                )
            {
                inputs.push((
                    path.clone(),
                    Some(fs::read(&path).map_err(|error| error.to_string())?),
                ));
            }
        }
        Ok(())
    }
    visit(root, &mut inputs)?;
    let mut seen = HashSet::new();
    inputs.retain(|(path, _)| seen.insert(identity(path)));
    inputs.sort_by_key(|(path, _)| identity(path));
    Ok(inputs)
}

fn verify(inputs: &[(PathBuf, Option<Vec<u8>>)]) -> Result<(), String> {
    for (path, expected) in inputs {
        let actual = match fs::read(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        if &actual != expected {
            return Err(format!(
                "original compilation input changed: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub(super) fn prepare(
    cli: &Cli,
    index: &SourceIndex,
    roots: &[String],
    progress: &mut impl FnMut(String),
) -> Result<Option<Original>, String> {
    let root = fs::canonicalize(cli.workspace_root.as_deref().unwrap_or(Path::new(".")))
        .map_err(|error| error.to_string())?;
    let wrapper = root.join(if cfg!(windows) {
        "gradlew.bat"
    } else {
        "gradlew"
    });
    if cli.validation_config.is_none() && !wrapper.is_file() {
        return Ok(None);
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stage = std::env::temp_dir().join(format!(
        "notlin-original-getters-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&stage).map_err(|error| error.to_string())?;
    let indexed_inputs = snapshots(index, &root)?;
    let mut inputs = indexed_inputs.clone();
    let mut explicit_config = None;
    let mut supporting = Vec::new();
    if let Some(path) = &cli.validation_config {
        inputs.push((
            path.clone(),
            Some(fs::read(path).map_err(|error| error.to_string())?),
        ));
        let config = crate::jvm_validation::load_validation_config(path, stage.join("classes"))?;
        let mut captured = inputs
            .iter()
            .map(|(path, _)| identity(path))
            .collect::<HashSet<_>>();
        for source in config.kotlin_sources.iter().chain(&config.java_sources) {
            if captured.insert(identity(source)) {
                inputs.push((
                    source.clone(),
                    Some(fs::read(source).map_err(|error| error.to_string())?),
                ));
            }
            if index.source_file(source).is_none() {
                let bytes = inputs
                    .iter()
                    .find(|(path, _)| identity(path) == identity(source))
                    .and_then(|(_, bytes)| bytes.as_ref())
                    .ok_or("configured supporting source snapshot missing")?;
                supporting.push(SourceOverlay::Replace {
                    path: source.clone(),
                    source: String::from_utf8(bytes.clone()).map_err(|error| error.to_string())?,
                    language: if source
                        .extension()
                        .is_some_and(|extension| extension == "java")
                    {
                        SourceLanguage::Java
                    } else {
                        SourceLanguage::Kotlin
                    },
                });
            }
        }
        explicit_config = Some(config);
    }
    verify(&inputs)?;
    let additional_inputs = inputs
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let contexts = if let Some(mut config) = explicit_config {
        config.run_main_class = None;
        // Do not permit a source-index plan to compile different on-disk input.
        let configured = config
            .kotlin_sources
            .iter()
            .chain(&config.java_sources)
            .map(|path| identity(path))
            .collect::<HashSet<_>>();
        if index
            .kotlin_files()
            .chain(index.java_files())
            .any(|file| !configured.contains(&identity(&file.path)))
        {
            return Err(
                "original getter compilation config does not cover the indexed workspace".into(),
            );
        }
        crate::jvm_validation::validate_jvm_sources(&config).map_err(|error| error.to_string())?;
        let mut classpath = config.classpath;
        classpath.insert(0, config.output_dir);
        let mut release = None;
        let mut args = config.javac_args.iter();
        while let Some(arg) = args.next() {
            match arg.to_string_lossy().as_ref() {
                "--release" => {
                    release = Some(
                        args.next()
                            .ok_or("missing javac release")?
                            .to_string_lossy()
                            .parse::<u16>()
                            .map_err(|error| error.to_string())?,
                    )
                }
                "-proc:none" | "-proc:full" | "-parameters" | "-Werror" | "-nowarn" => (),
                arg if arg.starts_with("-Xlint") => (),
                _ => {
                    return Err(
                        "unsupported javac arguments for original getter attribution".into(),
                    );
                }
            }
        }
        vec![(
            config.java_sources,
            JavaAnalysisContext {
                java: config.java,
                processors: lombok(&classpath),
                classpath,
                processor_path: Vec::new(),
                release,
                enable_preview: false,
                roots: roots.to_vec(),
                additional_inputs,
                timeout: Duration::from_secs(180),
            },
        )]
    } else {
        let script = stage.join("original.gradle");
        let output = stage.join("contexts.json");
        fs::write(&script, include_str!("original.gradle")).map_err(|error| error.to_string())?;
        let log = stage.join("gradle.log");
        let stdout = fs::File::create(&log).map_err(|error| error.to_string())?;
        let stderr = stdout.try_clone().map_err(|error| error.to_string())?;
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd.exe");
            command.args(["/d", "/c", "gradlew.bat"]);
            command
        } else {
            Command::new(&wrapper)
        };
        command
            .current_dir(&root)
            .arg("--init-script")
            .arg(&script)
            .arg(format!("-Dnotlin.getter.contexts={}", output.display()))
            .args([
                "notlinOriginalGetterContexts",
                "--no-daemon",
                "--no-configuration-cache",
                "--console=plain",
            ])
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        if let Some(jobs) = std::env::var_os("NOTLIN_JOBS") {
            command.arg("--max-workers").arg(jobs);
        }
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let started = Instant::now();
        let mut last_update = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(status)) => {
                    return Err(format!(
                        "original Gradle compilation failed ({status}); {}",
                        log.display()
                    ));
                }
                Ok(None) if started.elapsed() < Duration::from_secs(1800) => {
                    if last_update.elapsed() > Duration::from_secs(20) {
                        progress(format!(
                            "compiling original sources ({}s); {}",
                            started.elapsed().as_secs(),
                            log.display()
                        ));
                        last_update = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "original Gradle compilation interrupted: {result:?}; {}",
                        log.display()
                    ));
                }
            }
        }
        let exported: Vec<Context> =
            serde_json::from_slice(&fs::read(output).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        exported
            .into_iter()
            .filter_map(|context| {
                let paths = context
                    .sources
                    .iter()
                    .filter_map(|path| index.source_file(path).map(|file| file.path.clone()))
                    .collect::<Vec<_>>();
                (!paths.is_empty()).then(|| {
                    (
                        paths,
                        JavaAnalysisContext {
                            java: context.java,
                            processors: lombok(&context.processor_path),
                            classpath: context.classpath,
                            processor_path: context.processor_path,
                            release: Some(context.release),
                            enable_preview: context.enable_preview,
                            roots: roots.to_vec(),
                            additional_inputs: additional_inputs.clone(),
                            timeout: Duration::from_secs(180),
                        },
                    )
                })
            })
            .collect()
    };
    verify(&inputs)?;
    // Re-discovering catches source/config additions during the original build.
    if snapshots(index, &root)? != indexed_inputs {
        return Err("original compilation changed the configuration input set".into());
    }
    Ok(Some(Original {
        stage,
        contexts,
        snapshots: inputs,
        analysis_index: if supporting.is_empty() {
            None
        } else {
            Some(index.with_overlays(&supporting)?)
        },
    }))
}

fn lombok(classpath: &[PathBuf]) -> Vec<String> {
    if classpath.iter().any(|path| {
        path.file_name().is_some_and(|name| {
            name.to_string_lossy().starts_with("lombok-")
                && !name.to_string_lossy().contains("binding")
        })
    }) {
        vec!["lombok.launch.AnnotationProcessorHider$AnnotationProcessor".into()]
    } else {
        Vec::new()
    }
}
