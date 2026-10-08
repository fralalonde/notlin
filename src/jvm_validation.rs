//! Standalone validation for mixed Kotlin and Java source sets.
//!
//! Kotlin is compiled first while Java source files are supplied as symbol
//! inputs. Java is then compiled against the Kotlin output. An optional Java
//! main class can be launched to check the runtime boundary as well.

use serde::Deserialize;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone)]
pub struct JvmValidationConfig {
    /// Explicit path to the Kotlin compiler executable.
    pub kotlinc: PathBuf,
    /// Explicit path to the Java compiler executable.
    pub javac: PathBuf,
    /// Explicit path to the Java runtime executable.
    pub java: PathBuf,
    pub kotlin_sources: Vec<PathBuf>,
    pub java_sources: Vec<PathBuf>,
    /// Additional entries supplied to both compilers and the runtime.
    pub classpath: Vec<PathBuf>,
    pub output_dir: PathBuf,
    /// Optional fully qualified Java class to launch after successful compile.
    pub run_main_class: Option<String>,
    pub runtime_args: Vec<OsString>,
    /// Extra arguments passed verbatim to the Kotlin compiler.
    pub kotlinc_args: Vec<OsString>,
    /// Extra arguments passed verbatim to javac.
    pub javac_args: Vec<OsString>,
}

impl JvmValidationConfig {
    pub fn new(
        kotlinc: impl Into<PathBuf>,
        javac: impl Into<PathBuf>,
        java: impl Into<PathBuf>,
        output_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kotlinc: kotlinc.into(),
            javac: javac.into(),
            java: java.into(),
            kotlin_sources: Vec::new(),
            java_sources: Vec::new(),
            classpath: Vec::new(),
            output_dir: output_dir.into(),
            run_main_class: None,
            runtime_args: Vec::new(),
            kotlinc_args: Vec::new(),
            javac_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JvmValidationStage {
    KotlinCompile,
    JavaCompile,
    Runtime,
}

impl fmt::Display for JvmValidationStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::KotlinCompile => "Kotlin compilation",
            Self::JavaCompile => "Java compilation",
            Self::Runtime => "JVM execution",
        })
    }
}

#[derive(Debug, Clone)]
pub struct JvmCommandResult {
    pub stage: JvmValidationStage,
    pub command: PathBuf,
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Default)]
pub struct JvmValidationReport {
    pub commands: Vec<JvmCommandResult>,
}

#[derive(Debug)]
pub enum JvmValidationError {
    InvalidConfig(String),
    Io {
        stage: JvmValidationStage,
        command: PathBuf,
        source: std::io::Error,
    },
    Failed(JvmCommandResult),
}

impl fmt::Display for JvmValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid JVM validation config: {message}"),
            Self::Io {
                stage,
                command,
                source,
            } => {
                write!(
                    f,
                    "could not run {stage} command {}: {source}",
                    command.display()
                )
            }
            Self::Failed(result) => {
                write!(f, "{} failed", result.stage)?;
                if let Some(code) = result.status_code {
                    write!(f, " with exit code {code}")?;
                }
                let details = result.diagnostics();
                if !details.is_empty() {
                    write!(f, ":\n{details}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for JvmValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl JvmCommandResult {
    /// Compiler output in the order users expect to read it.
    pub fn diagnostics(&self) -> String {
        [self.stdout.trim(), self.stderr.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Compile a mixed source set in dependency order, optionally launching its
/// Java entry point. Paths are passed as OS arguments, so spaces and
/// non-UTF-8 paths are preserved.
pub fn validate_jvm_sources(
    config: &JvmValidationConfig,
) -> Result<JvmValidationReport, JvmValidationError> {
    if config.kotlin_sources.is_empty() && config.java_sources.is_empty() {
        return Err(JvmValidationError::InvalidConfig(
            "at least one Kotlin or Java source is required".into(),
        ));
    }
    if config
        .run_main_class
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(JvmValidationError::InvalidConfig(
            "runtime main class cannot be empty".into(),
        ));
    }
    for (tool, args) in [
        ("kotlinc", &config.kotlinc_args),
        ("javac", &config.javac_args),
    ] {
        if let Some(argument) = args.iter().map(|arg| arg.to_string_lossy()).find(|arg| {
            [
                "-d",
                "-classpath",
                "-cp",
                "--class-path",
                "--destination",
                "-destination",
            ]
            .iter()
            .any(|reserved| arg.as_ref() == *reserved || arg.starts_with(&format!("{reserved}=")))
        }) {
            return Err(JvmValidationError::InvalidConfig(format!(
                "{tool} argument `{argument}` cannot override the isolated output directory or classpath"
            )));
        }
    }
    // Validate path-list representability before creating output or launching
    // a tool. This catches embedded platform separators consistently.
    let _ = compiler_classpath(config, true)?;
    if config.run_main_class.is_some() {
        let _ = runtime_classpath(config)?;
    }

    std::fs::create_dir_all(&config.output_dir).map_err(|source| JvmValidationError::Io {
        stage: JvmValidationStage::KotlinCompile,
        command: config.kotlinc.clone(),
        source,
    })?;
    let mut output_entries =
        std::fs::read_dir(&config.output_dir).map_err(|source| JvmValidationError::Io {
            stage: JvmValidationStage::KotlinCompile,
            command: config.kotlinc.clone(),
            source,
        })?;
    if output_entries
        .next()
        .transpose()
        .map_err(|source| JvmValidationError::Io {
            stage: JvmValidationStage::KotlinCompile,
            command: config.kotlinc.clone(),
            source,
        })?
        .is_some()
    {
        return Err(JvmValidationError::InvalidConfig(format!(
            "output directory must be empty to avoid stale classes: {}",
            config.output_dir.display()
        )));
    }

    let mut report = JvmValidationReport::default();
    if !config.kotlin_sources.is_empty() {
        let mut command = Command::new(&config.kotlinc);
        command.args(&config.kotlinc_args);
        command.arg("-d").arg(&config.output_dir);
        let cp = compiler_classpath(config, true)?;
        if let Some(cp) = cp {
            command.arg("-classpath").arg(cp);
        }
        command.args(&config.kotlin_sources);
        // kotlinc reads Java sources for symbol resolution but emits only Kotlin
        // classes; javac performs the actual Java compilation below.
        command.args(&config.java_sources);
        run_stage(
            command,
            &config.kotlinc,
            JvmValidationStage::KotlinCompile,
            &mut report,
        )?;
    }

    if !config.java_sources.is_empty() {
        let mut command = Command::new(&config.javac);
        command.args(&config.javac_args);
        command.arg("-d").arg(&config.output_dir);
        if let Some(cp) = compiler_classpath(config, true)? {
            command.arg("-classpath").arg(cp);
        }
        command.args(&config.java_sources);
        run_stage(
            command,
            &config.javac,
            JvmValidationStage::JavaCompile,
            &mut report,
        )?;
    }

    if let Some(main_class) = &config.run_main_class {
        let mut command = Command::new(&config.java);
        command.arg("-classpath");
        command.arg(runtime_classpath(config)?);
        command.arg(main_class);
        command.args(&config.runtime_args);
        run_stage(
            command,
            &config.java,
            JvmValidationStage::Runtime,
            &mut report,
        )?;
    }
    Ok(report)
}

/// User-facing versioned JSON form. Source/tool/classpath paths are resolved
/// relative to the config file, so configs remain portable as a unit.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidationFile {
    version: u32,
    kotlinc: PathBuf,
    javac: PathBuf,
    java: PathBuf,
    #[serde(default)]
    kotlin_sources: Vec<PathBuf>,
    #[serde(default)]
    java_sources: Vec<PathBuf>,
    #[serde(default)]
    classpath: Vec<PathBuf>,
    #[serde(default)]
    kotlinc_args: Vec<String>,
    #[serde(default)]
    javac_args: Vec<String>,
}

pub fn load_validation_config(
    path: &Path,
    output_dir: PathBuf,
) -> Result<JvmValidationConfig, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut raw: ValidationFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if raw.version != 1 {
        return Err(format!(
            "unsupported validation config version {} (expected 1)",
            raw.version
        ));
    }
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let resolve = |p: PathBuf| if p.is_absolute() { p } else { base.join(p) };
    Ok(JvmValidationConfig {
        kotlinc: resolve(raw.kotlinc),
        javac: resolve(raw.javac),
        java: resolve(raw.java),
        kotlin_sources: raw.kotlin_sources.drain(..).map(&resolve).collect(),
        java_sources: raw.java_sources.drain(..).map(&resolve).collect(),
        classpath: raw.classpath.drain(..).map(&resolve).collect(),
        output_dir,
        run_main_class: None,
        runtime_args: Vec::new(),
        kotlinc_args: raw.kotlinc_args.into_iter().map(OsString::from).collect(),
        javac_args: raw.javac_args.into_iter().map(OsString::from).collect(),
    })
}

fn compiler_classpath(
    config: &JvmValidationConfig,
    include_output: bool,
) -> Result<Option<OsString>, JvmValidationError> {
    let mut entries = config.classpath.clone();
    if include_output && (!config.kotlin_sources.is_empty() || !config.java_sources.is_empty()) {
        entries.insert(0, config.output_dir.clone());
    }
    join_classpath(&entries)
}

fn runtime_classpath(config: &JvmValidationConfig) -> Result<OsString, JvmValidationError> {
    let mut entries = vec![config.output_dir.clone()];
    entries.extend(config.classpath.iter().cloned());
    join_classpath(&entries)?.ok_or_else(|| {
        JvmValidationError::InvalidConfig("runtime classpath unexpectedly empty".into())
    })
}

fn join_classpath(entries: &[PathBuf]) -> Result<Option<OsString>, JvmValidationError> {
    if entries.is_empty() {
        return Ok(None);
    }
    std::env::join_paths(entries).map(Some).map_err(|error| {
        JvmValidationError::InvalidConfig(format!("invalid classpath entry: {error}"))
    })
}

fn run_stage(
    mut command: Command,
    command_path: &Path,
    stage: JvmValidationStage,
    report: &mut JvmValidationReport,
) -> Result<(), JvmValidationError> {
    let output = command.output().map_err(|source| JvmValidationError::Io {
        stage,
        command: command_path.to_path_buf(),
        source,
    })?;
    let result = command_result(stage, command_path, output);
    if result.status_code != Some(0) {
        return Err(JvmValidationError::Failed(result));
    }
    report.commands.push(result);
    Ok(())
}

fn command_result(stage: JvmValidationStage, command: &Path, output: Output) -> JvmCommandResult {
    JvmCommandResult {
        stage,
        command: command.to_path_buf(),
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}
