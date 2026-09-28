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
    // Kotlin sources nest deeply — chained builders, large `when` subjects,
    // long string templates with interpolated expressions — and the expression
    // walker is recursive, so a real-world file can exhaust the default stack
    // and abort with "has overflowed its stack" (no diagnostic, no file name).
    // Give the run a stack sized for the deepest input we accept.
    // `NOTLIN_STACK_MB` overrides it: an input that still overflows a very
    // large stack is a recursion cycle, not merely deep nesting, and is a bug
    // to reproduce rather than a limit to raise again.
    let stack_mb = std::env::var("NOTLIN_STACK_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(512);
    std::thread::Builder::new()
        .name("notlin".into())
        .stack_size(stack_mb * 1024 * 1024)
        .spawn(run_cli)
        .expect("spawn the translator thread")
        .join()
        .unwrap_or(ExitCode::FAILURE)
}

fn run_cli() -> ExitCode {
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

/// Post-migration manual-marker pass: scan the retained `.kt` text and insert
/// `NOTLIN-MANUAL:` comments where a small user edit unlocks more translations
/// on the next run (Java-property smart-cast bundles, Kotlin `copy()` calls
/// against translated data classes). Idempotent; count is logged.
fn annotate_manual_marks(file: &Path, outcome: &MigrateOutcome, index: &SourceIndex) -> usize {
    if matches!(outcome, MigrateOutcome::Deleted) {
        return 0;
    }
    if file.extension().and_then(|e| e.to_str()) != Some("kt") {
        return 0;
    }
    let marks = notlin::manual_marks::annotate_manual_spots(file, index);
    if marks > 0 {
        log::info!(
            "{}: {} NOTLIN-MANUAL spot(s) flagged — user edits unlock further translation",
            notlin::paths::display(file),
            marks
        );
    }
    marks
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    let workspace_root = cli
        .workspace_root
        .clone()
        .unwrap_or(std::env::current_dir().map_err(|e| format!("current directory: {e}"))?);
    let workspace_root = std::fs::canonicalize(&workspace_root)
        .map_err(|e| format!("workspace root {}: {e}", notlin::paths::display(workspace_root)))?;
    // Every path printed from here on — diagnostics, logs, the run summary,
    // the retention table — is relative to this root.
    notlin::paths::set_root(&workspace_root);
    let (index, index_stats) = SourceIndex::discover_with_stats(&workspace_root)?;
    log::debug!(
        "indexed {} Kotlin and {} Java files from {} ({} parsed, {} cached)",
        index.kotlin_files().count(),
        index.java_files().count(),
        notlin::paths::display(workspace_root),
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
                    .map_err(|e| format!("translation root {}: {e}", notlin::paths::display(input)))
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
            // Tolerant read: a source discovered by collect_inputs can VANISH
            // before read_source (a concurrent cleanup, an overlapping
            // notlin run migrating the same tree, or an External editor).
            // Skip it with a printed warning instead of aborting the whole
            // workspace migration — the file stays Kotlin and the NEXT run
            // sees it again if it reappears.
            let mut sources: Vec<(PathBuf, String)> = Vec::with_capacity(files.len());
            let mut missing = 0usize;
            for file in &files {
                match read_source(file) {
                    Ok(source) => sources.push((file.clone(), source)),
                    Err(reason) => {
                        missing += 1;
                        eprintln!(
                            "{}",
                            format!("warning: source unreadable, skipped: {reason}").yellow()
                        );
                    }
                }
            }
            if sources.is_empty() && missing > 0 {
                return Err("all input files disappeared before reading".into());
            }
            let files = sources.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>();
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
                    &plan.java_files,
                )?;
                match &outcome {
                    migrate::MigrateOutcome::Deleted => {
                        log::debug!("deleted {}", notlin::paths::display(plan.file));
                    }
                    migrate::MigrateOutcome::Trimmed { remaining_bytes } => {
                        log::debug!(
                            "trimmed {} ({remaining_bytes} bytes remain)",
                            notlin::paths::display(plan.file)
                        );
                    }
                    migrate::MigrateOutcome::Untouched => {
                        log::debug!("{}: no translated content; untouched", notlin::paths::display(plan.file));
                    }
                }
                outcomes.push((plan.file.clone(), outcome));
            }
            // Migration writes Java after the initial Kotlin/Java index was
            // built. Re-index once before repairing retained Kotlin so its
            // Java getter boundaries resolve against the generated sources,
            // not their pre-migration Kotlin declarations.
            let migrated_index = SourceIndex::discover(&workspace_root)?;
            for (file, outcome) in &outcomes {
                annotate_manual_marks(file, outcome, &migrated_index);
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
        log::debug!("transpiling {}", notlin::paths::display(file));
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

        // Output dir precedence: explicit -o, else a workspace migration writes
        // next to the input, else stdout.
        let effective_out_dir = match (&cli.out_dir, cli.migrates_in_place()) {
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
                            .map_err(|e| format!("{}: {e}", notlin::paths::display(parent)))?;
                    }
                    let file = std::fs::File::create(&target)
                        .map_err(|e| format!("{}: {e}", notlin::paths::display(target)))?;
                    let mut writer = BufWriter::new(file);
                    writer
                        .write_all(content.as_bytes())
                        .map_err(|e| format!("{}: {e}", notlin::paths::display(target)))?;
                    writer
                        .flush()
                        .map_err(|e| format!("{}: {e}", notlin::paths::display(target)))?;
                    log::debug!("wrote {}", notlin::paths::display(target));
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
        if cli.migrates_in_place() && !cli.dump_ast {
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
                        log::debug!("deleted {}", notlin::paths::display(file));
                    }
                    migrate::MigrateOutcome::Trimmed { remaining_bytes } => {
                        log::debug!(
                            "trimmed {} ({remaining_bytes} bytes remain)",
                            notlin::paths::display(file)
                        );
                    }
                    migrate::MigrateOutcome::Untouched => {
                        log::debug!("{}: no translated content; untouched", notlin::paths::display(file));
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
                            let source_text = source_canon.to_string_lossy().replace('\\', "/");
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
                                        log::info!("deleted orphan {}", notlin::paths::display(path));
                                    }
                                }
                            }
                        }
                    }
                }
                annotate_manual_marks(file, &outcome, &index);
                outcomes.push((file.clone(), outcome));
            } else {
                log::info!(
                    "{}: kept — run had errors/warnings in --untranslatable=error mode",
                    notlin::paths::display(file)
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
    print_retention_report();

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Write one plan's java files: explicit -o dir, else next to the input during
/// a workspace migration, else stdout. Mirrors the per-file loop's precedence.
fn write_java_files(
    cli: &Cli,
    file: &Path,
    java_files: &[(String, String)],
    stdout: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<(), String> {
    let effective_out_dir = match (&cli.out_dir, cli.migrates_in_place()) {
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
                        .map_err(|e| format!("{}: {e}", notlin::paths::display(parent)))?;
                }
                let out = std::fs::File::create(&target)
                    .map_err(|e| format!("{}: {e}", notlin::paths::display(target)))?;
                let mut writer = BufWriter::new(out);
                writer
                    .write_all(content.as_bytes())
                    .map_err(|e| format!("{}: {e}", notlin::paths::display(target)))?;
                writer
                    .flush()
                    .map_err(|e| format!("{}: {e}", notlin::paths::display(target)))?;
                log::debug!("wrote {}", notlin::paths::display(target));
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

/// Workspace migration for one plan: strip translated declarations, delete
/// fully-translated files, orphan-cleanup retained ones. Mirrors the per-file
/// loop's migration block.
fn migrate_file(
    cli: &Cli,
    file: &Path,
    source: &str,
    errors: usize,
    warnings: usize,
    coverage: &notlin::diagnostics::FileCoverage,
    emitted_java_files: &[(String, String)],
) -> Result<MigrateOutcome, String> {
    if !cli.migrates_in_place() || cli.dump_ast {
        return Ok(MigrateOutcome::Untouched);
    }
    let strict_block =
        matches!(cli.untranslatable, UntranslatableMode::Error) && (errors > 0 || warnings > 0);
    if strict_block {
        log::info!(
            "{}: kept — run had errors/warnings in --untranslatable=error mode",
            notlin::paths::display(file)
        );
        return Ok(MigrateOutcome::Untouched);
    }
    let outcome = migrate::migrate(file, source, coverage)?;
    // A source can be partially translated: some declarations are emitted as
    // Java while a later fixpoint pass retains others in Kotlin. Remove only
    // generated siblings no longer emitted by THIS source; otherwise javac
    // compiles an obsolete Java class beside the retained Kotlin declaration.
    if cli.out_dir.is_none()
        && let Some(dir) = file.parent()
    {
        let emitted: HashSet<&str> = emitted_java_files
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        let source_canon = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        let source_text = source_canon.to_string_lossy().replace('\\', "/");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("java")
                    || emitted.contains(
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default(),
                    )
                {
                    continue;
                }
                if let Ok(first_line) = std::fs::read_to_string(&path)
                    .map(|content| content.lines().next().unwrap_or("").to_string())
                    && first_line.contains("NOTLIN: generated from")
                    && first_line.contains(&source_text)
                {
                    std::fs::remove_file(&path)
                        .map_err(|error| format!("{}: {error}", notlin::paths::display(path)))?;
                    log::info!("deleted stale generated output {}", notlin::paths::display(path));
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
    print_retention_report();
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Run-end retention table: what stayed Kotlin, why, how much of it, and where
/// a human has to decide. Part of the run summary, printed before exit.
fn print_retention_report() {
    if let Some(report) = notlin::diagnostics::retention_report() {
        eprint!("{report}");
    }
}

fn read_source(file: &Path) -> Result<String, String> {
    let mut source = String::new();
    if file.as_os_str() == "-" {
        let stdin = std::io::stdin();
        BufReader::new(stdin.lock())
            .read_to_string(&mut source)
            .map_err(|e| format!("stdin: {e}"))?;
    } else {
        let input = std::fs::File::open(file).map_err(|e| format!("{}: {e}", notlin::paths::display(file)))?;
        BufReader::new(input)
            .read_to_string(&mut source)
            .map_err(|e| format!("{}: {e}", notlin::paths::display(file)))?;
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
                .map_err(|e| format!("input directory {}: {e}", notlin::paths::display(input)))?;
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
            std::fs::read_dir(&directory).map_err(|e| format!("{}: {e}", notlin::paths::display(directory)))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", notlin::paths::display(directory)))?;
            let path = entry.path();
            let is_kotlin = path.extension().is_some_and(|extension| extension == "kt");
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) if !is_kotlin => continue,
                Err(e) => return Err(format!("{}: {e}", notlin::paths::display(path))),
            };
            if metadata.is_dir() {
                let canonical =
                    std::fs::canonicalize(&path).map_err(|e| format!("{}: {e}", notlin::paths::display(path)))?;
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
