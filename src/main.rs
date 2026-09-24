use clap::Parser as _;
use colored::Colorize;
use notlin::cli::{Cli, UntranslatableMode};
use notlin::migrate::{self, MigrateOutcome};
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::collections::HashSet;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    env_logger::Builder::new()
        .filter_level(match cli.verbose {
            0 => log::LevelFilter::Warn,
            1 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        })
        .init();
    log::debug!("cli: {:?}", cli);

    match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}", format!("fatal: {e}").red());
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    let workspace_root = cli
        .workspace_root
        .clone()
        .unwrap_or(std::env::current_dir().map_err(|e| format!("current directory: {e}"))?);
    let workspace_root = std::fs::canonicalize(&workspace_root)
        .map_err(|e| format!("workspace root {}: {e}", workspace_root.display()))?;
    let (index, index_stats) = SourceIndex::discover_with_stats(&workspace_root)?;
    log::debug!(
        "indexed {} Kotlin and {} Java files from {} ({} parsed, {} cached)",
        index.kotlin_files().count(),
        index.java_files().count(),
        workspace_root.display(),
        index_stats.parsed_files,
        index_stats.reused_files,
    );

    let translation_roots = if cli.input.is_empty() {
        vec![workspace_root.clone()]
    } else {
        cli.input
            .iter()
            .filter(|input| input.as_os_str() != "-")
            .map(|input| {
                log::debug!("source root {}", input.to_string_lossy());
                std::fs::canonicalize(input)
                    .map_err(|e| format!("translation root {}: {e}", input.display()))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let files = collect_inputs(&cli.input)?;
    if files.is_empty() {
        return Err("no input files given (positional <INPUT>..., or use - for stdin)".into());
    }
    let stdout = std::io::stdout();
    let mut stdout = BufWriter::new(stdout.lock());

    let mut total_errors = 0usize;
    let mut total_warnings = 0usize;
    let mut java_written = 0usize;
    let mut outcomes: Vec<(PathBuf, MigrateOutcome)> = Vec::new();

    // Workspace mode (index + more than one file): compute the retention
    // fixpoint first (probe passes in memory, no writes, no printed
    // diagnostics), then write each file's plan exactly once. The subtype
    // retention rule then only fires for subtypes that are THEMSELVES
    // retained, so clean hub-and-implementor families translate together.
    if cli.workspace_root.is_some() || files.len() > 1 {
        // Read every source up front; stdin ('-') cannot participate in a
        // multi-file fixpoint (no path to index), so it keeps the old path.
        if files.iter().all(|f| f.as_os_str() != "-") {
            let sources: Vec<(PathBuf, String)> = files
                .iter()
                .map(|file| read_source(file).map(|source| (file.clone(), source)))
                .collect::<Result<_, _>>()?;
            let plans =
                transpiler::fixpoint::plan_workspace(&sources, cli, &index, &translation_roots, 16);
            for plan in &plans {
                total_errors += plan.errors;
                total_warnings += plan.warnings;
                java_written += plan.java_files.len();
                write_java_files(cli, &plan.file, &plan.java_files, &mut stdout)?;
                let outcome = migrate_file(
                    cli,
                    &plan.file,
                    &plan.source,
                    plan.errors,
                    plan.warnings,
                    &plan.coverage,
                )?;
                match &outcome {
                    migrate::MigrateOutcome::Deleted => {
                        log::debug!("deleted {}", plan.file.display());
                    }
                    migrate::MigrateOutcome::Trimmed { remaining_bytes } => {
                        log::debug!(
                            "trimmed {} ({remaining_bytes} bytes remain)",
                            plan.file.display()
                        );
                    }
                    migrate::MigrateOutcome::Untouched => {
                        log::debug!("{}: no translated content; untouched", plan.file.display());
                    }
                }
                outcomes.push((plan.file.clone(), outcome));
            }
            return finish_run(
                cli,
                files.len(),
                total_errors,
                total_warnings,
                java_written,
                &outcomes,
                &mut stdout,
            );
        }
    }

    for file in &files {
        log::debug!("transpiling {}", file.display());
        let source = read_source(file)?;

        if cli.dump_ast {
            stdout
                .write_all(transpiler::dump_ast(&source).as_bytes())
                .map_err(|error| format!("stdout: {error}"))?;
            continue;
        }

        let (java_files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
            &source,
            file,
            cli,
            Some(&index),
            &translation_roots,
        );
        total_errors += errors;
        total_warnings += warnings;

        // Output dir precedence: explicit -o, else --in-place writes next to
        // the input, else stdout.
        let effective_out_dir = match (&cli.out_dir, cli.in_place) {
            (Some(d), _) => Some(d.clone()),
            (None, true) => file.parent().map(|p| p.to_path_buf()),
            (None, false) => None,
        };

        match &effective_out_dir {
            Some(out_dir) => {
                for (name, content) in &java_files {
                    let target = out_dir.join(name);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| format!("{}: {e}", parent.display()))?;
                    }
                    let file = std::fs::File::create(&target)
                        .map_err(|e| format!("{}: {e}", target.display()))?;
                    let mut writer = BufWriter::new(file);
                    writer
                        .write_all(content.as_bytes())
                        .map_err(|e| format!("{}: {e}", target.display()))?;
                    writer
                        .flush()
                        .map_err(|e| format!("{}: {e}", target.display()))?;
                    log::debug!("wrote {}", target.display());
                }
                java_written += java_files.len();
            }
            None => {
                for (name, content) in &java_files {
                    if java_files.len() > 1 {
                        writeln!(stdout, "// ===== {} =====", name)
                            .map_err(|error| format!("stdout: {error}"))?;
                    }
                    stdout
                        .write_all(content.as_bytes())
                        .map_err(|error| format!("stdout: {error}"))?;
                }
            }
        }

        // --in-place: strip translated declarations from the .kt file;
        // delete it when fully translated. Files with only untranslatable
        // content are untouched. Dump-ast mode never mutates input.
        if cli.in_place && !cli.dump_ast {
            // In strict (--untranslatable=error) mode an untranslatable makes
            // the run fail; never delete/trim input on such a run. In warn
            // mode untranslatables don't block migration — the taint system
            // already kept them in the .kt file.
            let strict_block = matches!(cli.untranslatable, UntranslatableMode::Error)
                && (errors > 0 || warnings > 0);
            if !strict_block {
                let outcome = migrate::migrate(file, &source, &coverage)?;
                match &outcome {
                    migrate::MigrateOutcome::Deleted => {
                        log::debug!("deleted {}", file.display());
                    }
                    migrate::MigrateOutcome::Trimmed { remaining_bytes } => {
                        log::debug!(
                            "trimmed {} ({remaining_bytes} bytes remain)",
                            file.display()
                        );
                    }
                    migrate::MigrateOutcome::Untouched => {
                        log::debug!("{}: no translated content; untouched", file.display());
                        // Orphan cleanup: a prior run may have generated Java
                        // outputs whose declarations this run retained in
                        // Kotlin. Generated outputs are marked with the
                        // `NOTLIN: generated from <source>` header — delete
                        // those whose source is THIS file, otherwise javac
                        // keeps compiling a stale class that no longer
                        // matches the retained Kotlin ABI. Generated outputs
                        // land next to the source in in-place mode.
                        if effective_out_dir.is_none()
                            && let Some(dir) = file.parent()
                        {
                            let source_canon =
                                std::fs::canonicalize(file).unwrap_or_else(|_| file.clone());
                            let source_text = source_canon.to_string_lossy().to_string();
                            if let Ok(entries) = std::fs::read_dir(dir) {
                                for entry in entries.flatten() {
                                    let path = entry.path();
                                    if path.extension().and_then(|e| e.to_str()) != Some("java") {
                                        continue;
                                    }
                                    if let Ok(first_line) =
                                        std::fs::read_to_string(&path).map(|content| {
                                            content.lines().next().unwrap_or("").to_string()
                                        })
                                        && first_line.contains("NOTLIN: generated from")
                                        && first_line.contains(&source_text)
                                    {
                                        let _ = std::fs::remove_file(&path);
                                        log::info!("deleted orphan {}", path.display());
                                    }
                                }
                            }
                        }
                    }
                }
                outcomes.push((file.clone(), outcome));
            } else {
                log::info!(
                    "{}: kept — run had errors/warnings in --untranslatable=error mode",
                    file.display()
                );
                outcomes.push((file.clone(), MigrateOutcome::Untouched));
            }
        }
    }

    let untranslatable_strict = matches!(cli.untranslatable, UntranslatableMode::Error);
    let failed = total_errors > 0
        || (untranslatable_strict && total_warnings > 0)
        || (cli.deny_warnings && total_warnings > 0);

    let deleted = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, MigrateOutcome::Deleted))
        .count();
    let trimmed = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, MigrateOutcome::Trimmed { .. }))
        .count();
    let untouched = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, MigrateOutcome::Untouched))
        .count();
    stdout.flush().map_err(|error| format!("stdout: {error}"))?;
    eprintln!(
        "notlin: {} file(s) processed, {} java file(s) written, {} error(s), {} warning(s){} — {}",
        files.len(),
        java_written,
        total_errors,
        total_warnings,
        if outcomes.is_empty() {
            String::new()
        } else {
            format!(", migration: {deleted} deleted, {trimmed} trimmed, {untouched} untouched")
        },
        if failed {
            "failed".red()
        } else {
            "success".green()
        }
    );

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Write one plan's java files: explicit -o dir, else next to the input
/// (--in-place), else stdout. Mirrors the per-file loop's output precedence.
fn write_java_files(
    cli: &Cli,
    file: &Path,
    java_files: &[(String, String)],
    stdout: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), String> {
    let effective_out_dir = match (&cli.out_dir, cli.in_place) {
        (Some(d), _) => Some(d.clone()),
        (None, true) => file.parent().map(|p| p.to_path_buf()),
        (None, false) => None,
    };
    match &effective_out_dir {
        Some(out_dir) => {
            for (name, content) in java_files {
                let target = out_dir.join(name);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                let out = std::fs::File::create(&target)
                    .map_err(|e| format!("{}: {e}", target.display()))?;
                let mut writer = BufWriter::new(out);
                writer
                    .write_all(content.as_bytes())
                    .map_err(|e| format!("{}: {e}", target.display()))?;
                writer
                    .flush()
                    .map_err(|e| format!("{}: {e}", target.display()))?;
                log::debug!("wrote {}", target.display());
            }
        }
        None => {
            for (name, content) in java_files {
                if java_files.len() > 1 {
                    writeln!(stdout, "// ===== {name} =====")
                        .map_err(|error| format!("stdout: {error}"))?;
                }
                stdout
                    .write_all(content.as_bytes())
                    .map_err(|error| format!("stdout: {error}"))?;
            }
        }
    }
    Ok(())
}

/// --in-place migration for one plan: strip translated declarations, delete
/// fully-translated files, orphan-cleanup retained ones. Mirrors the
/// per-file loop's migration block.
fn migrate_file(
    cli: &Cli,
    file: &Path,
    source: &str,
    errors: usize,
    warnings: usize,
    coverage: &notlin::diagnostics::FileCoverage,
) -> Result<MigrateOutcome, String> {
    if !cli.in_place || cli.dump_ast {
        return Ok(MigrateOutcome::Untouched);
    }
    let strict_block =
        matches!(cli.untranslatable, UntranslatableMode::Error) && (errors > 0 || warnings > 0);
    if strict_block {
        log::info!(
            "{}: kept — run had errors/warnings in --untranslatable=error mode",
            file.display()
        );
        return Ok(MigrateOutcome::Untouched);
    }
    let outcome = migrate::migrate(file, source, coverage)?;
    if matches!(outcome, MigrateOutcome::Untouched) {
        // Orphan cleanup: a prior run may have generated Java outputs whose
        // declarations this run retained in Kotlin. Generated outputs are
        // marked with the `NOTLIN: generated from <source>` header — delete
        // those whose source is THIS file, otherwise javac keeps compiling
        // a stale class that no longer matches the retained Kotlin ABI.
        // Generated outputs land next to the source in in-place mode.
        let has_out_dir = cli.out_dir.is_some();
        if !has_out_dir && let Some(dir) = file.parent() {
            let source_canon = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
            let source_text = source_canon.to_string_lossy().to_string();
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("java") {
                        continue;
                    }
                    if let Ok(first_line) = std::fs::read_to_string(&path)
                        .map(|content| content.lines().next().unwrap_or("").to_string())
                        && first_line.contains("NOTLIN: generated from")
                        && first_line.contains(&source_text)
                    {
                        let _ = std::fs::remove_file(&path);
                        log::info!("deleted orphan {}", path.display());
                    }
                }
            }
        }
    }
    Ok(outcome)
}

/// Shared run summary: failure policy, migration tally, final line.
fn finish_run(
    cli: &Cli,
    file_count: usize,
    total_errors: usize,
    total_warnings: usize,
    java_written: usize,
    outcomes: &[(PathBuf, MigrateOutcome)],
    stdout: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<ExitCode, String> {
    let untranslatable_strict = matches!(cli.untranslatable, UntranslatableMode::Error);
    let failed = total_errors > 0
        || (untranslatable_strict && total_warnings > 0)
        || (cli.deny_warnings && total_warnings > 0);
    let deleted = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, MigrateOutcome::Deleted))
        .count();
    let trimmed = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, MigrateOutcome::Trimmed { .. }))
        .count();
    let untouched = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, MigrateOutcome::Untouched))
        .count();
    stdout.flush().map_err(|error| format!("stdout: {error}"))?;
    eprintln!(
        "notlin: {} file(s) processed, {} java file(s) written, {} error(s), {} warning(s){} — {}",
        file_count,
        java_written,
        total_errors,
        total_warnings,
        if outcomes.is_empty() {
            String::new()
        } else {
            format!(", migration: {deleted} deleted, {trimmed} trimmed, {untouched} untouched")
        },
        if failed {
            "failed".red()
        } else {
            "success".green()
        }
    );
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn read_source(file: &Path) -> Result<String, String> {
    let mut source = String::new();
    if file.as_os_str() == "-" {
        let stdin = std::io::stdin();
        BufReader::new(stdin.lock())
            .read_to_string(&mut source)
            .map_err(|e| format!("stdin: {e}"))?;
    } else {
        let input = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
        BufReader::new(input)
            .read_to_string(&mut source)
            .map_err(|e| format!("{}: {e}", file.display()))?;
    }
    Ok(source)
}

fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for input in inputs {
        if input.as_os_str() == "-" {
            files.push(input.clone());
            continue;
        }
        if input.is_dir() {
            let canonical = std::fs::canonicalize(input)
                .map_err(|e| format!("input directory {}: {e}", input.display()))?;
            let mut dir_files = walk_kotlin(&canonical)?;
            dir_files.sort();
            files.extend(dir_files);
        } else {
            files.push(input.clone());
        }
    }
    let mut seen = HashSet::new();
    files.retain(|file| {
        if file.as_os_str() == "-" {
            return seen.insert("-".to_string());
        }
        let identity = std::fs::canonicalize(file).unwrap_or_else(|_| file.clone());
        let identity = identity.to_string_lossy().replace('\\', "/");
        let identity = if cfg!(windows) {
            identity.to_ascii_lowercase()
        } else {
            identity
        };
        seen.insert(identity)
    });
    log::debug!("collected {} .kt files", files.len());
    Ok(files)
}

fn walk_kotlin(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut visited_directories = HashSet::from([dir.to_path_buf()]);
    while let Some(directory) = stack.pop() {
        let entries =
            std::fs::read_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", directory.display()))?;
            let path = entry.path();
            let is_kotlin = path.extension().is_some_and(|extension| extension == "kt");
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) if !is_kotlin => continue,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            };
            if metadata.is_dir() {
                let canonical =
                    std::fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                if visited_directories.insert(canonical.clone()) {
                    stack.push(canonical);
                }
            } else if metadata.is_file() && is_kotlin {
                out.push(path);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::collect_inputs;
    use std::fs;

    #[test]
    fn duplicate_input_paths_are_processed_once() {
        let root = std::env::temp_dir().join(format!("notlin-input-dedupe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("Sample.kt");
        fs::write(&source, "class Sample\n").unwrap();

        let files = collect_inputs(&[source.clone(), source.clone()]).unwrap();
        assert_eq!(files, vec![source]);
        fs::remove_dir_all(root).unwrap();
    }
}
