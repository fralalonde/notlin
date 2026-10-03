use clap::Parser as _;
use colored::Colorize;
use notlin::cli::{Cli, UntranslatableMode};
use notlin::migrate::{self, MigrateOutcome};
use notlin::transpiler;
use notlin::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn progress_start(phase: &str, detail: impl std::fmt::Display) {
    let label = format!("{phase:<11}");
    eprintln!(
        "  {} {}{}",
        "◇".cyan(),
        label.bold(),
        detail.to_string().bright_black()
    );
}

fn progress_activity(phase: &str, detail: impl std::fmt::Display) {
    let label = format!("{phase:<11}");
    eprintln!(
        "  {} {}{}",
        "·".bright_black(),
        label.bold(),
        detail.to_string().bright_black()
    );
}

fn progress_done(phase: &str, detail: impl std::fmt::Display) {
    let label = format!("{phase:<11}");
    eprintln!(
        "  {} {}{}",
        "✓".green(),
        label.bold(),
        detail.to_string().bright_black()
    );
}

fn item_count(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn print_run_summary(
    file_count: usize,
    total_errors: usize,
    total_warnings: usize,
    java_written: usize,
    outcomes: &[(PathBuf, MigrateOutcome)],
    failed: bool,
) {
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

    eprintln!();
    eprintln!("{}", "notlin summary".bold());
    eprintln!(
        "  {} {:<11} {}",
        "├─".bright_black(),
        "sources".bold(),
        format!("{file_count} processed").bright_black()
    );
    eprintln!(
        "  {} {:<11} {}",
        "├─".bright_black(),
        "java".bold(),
        format!("{java_written} written").bright_black()
    );
    if !outcomes.is_empty() {
        eprintln!(
            "  {} {:<11} {}",
            "├─".bright_black(),
            "migration".bold(),
            format!("{deleted} deleted · {trimmed} trimmed · {untouched} untouched").bright_black()
        );
    }
    eprintln!(
        "  {} {:<11} {}",
        "├─".bright_black(),
        "diagnostics".bold(),
        format!("{total_errors} errors · {total_warnings} warnings").bright_black()
    );
    eprintln!(
        "  {} {:<11} {}",
        "└─".bright_black(),
        "result".bold(),
        if failed {
            "failed".red().bold()
        } else {
            "success".green().bold()
        }
    );
}

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
        .format(|buffer, record| writeln!(buffer, "  {} {}", "│".bright_black(), record.args()))
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
    let workspace_root = std::fs::canonicalize(&workspace_root).map_err(|e| {
        format!(
            "workspace root {}: {e}",
            notlin::paths::display(&workspace_root)
        )
    })?;
    // Every path printed from here on — diagnostics, logs, the run summary,
    // the retention table — is relative to this root.
    notlin::paths::set_root(&workspace_root);
    eprintln!("{}", "notlin".bold());
    progress_start("index", "scanning workspace");
    let (index, index_stats) = SourceIndex::discover_with_stats(&workspace_root)?;
    progress_done(
        "index",
        format!(
            "{} · {} · {} parsed · {} cached",
            item_count(index.kotlin_files().count(), "Kotlin file", "Kotlin files"),
            item_count(index.java_files().count(), "Java file", "Java files"),
            index_stats.parsed_files,
            index_stats.reused_files
        ),
    );
    log::debug!(
        "indexed {} Kotlin and {} Java files from {} ({} parsed, {} cached)",
        index.kotlin_files().count(),
        index.java_files().count(),
        notlin::paths::display(&workspace_root),
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
    progress_start("input", "discovering Kotlin sources");
    let files = collect_inputs(&cli.input)?;
    progress_done(
        "input",
        format!("{} selected", item_count(files.len(), "source", "sources")),
    );
    let stdout = std::io::stdout();
    let mut stdout = BufWriter::new(stdout.lock());
    if files.is_empty() {
        if !cli.input.is_empty() && cli.input.iter().all(|input| input.is_dir()) {
            return finish_run(cli, 0, 0, 0, 0, &[], &mut stdout);
        }
        return Err("no input files given (positional <INPUT>..., or use - for stdin)".into());
    }

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
            progress_start(
                "plan",
                format!("resolving {}", item_count(files.len(), "source", "sources")),
            );
            let mut virtual_sources = sources.clone();
            let mut cumulative: HashMap<PathBuf, (PathBuf, String, String)> = HashMap::new();
            let declaration_count = index
                .kotlin_files()
                .map(|f| f.declarations.len())
                .sum::<usize>();
            let limit = declaration_count.saturating_mul(2).max(16);
            let mut converged_plans = None;
            let mut converged_round = 0usize;
            let mut retained_seed: Option<HashSet<String>> = None;
            let profile_speculation = std::env::var_os("NOTLIN_PROFILE").is_some();
            for round in 1..=limit {
                notlin::diagnostics::clear_retention();
                let mut overlays = Vec::new();
                for (path, _) in &sources {
                    if !virtual_sources.iter().any(|(current, _)| current == path) {
                        overlays.push(SourceOverlay::Delete { path: path.clone() });
                    }
                }
                for (path, source) in &virtual_sources {
                    if let Some((_, original_source)) = sources.iter().find(|(p, _)| p == path)
                        && source != original_source
                    {
                        overlays.push(SourceOverlay::Replace {
                            path: path.clone(),
                            language: SourceLanguage::Kotlin,
                            source: source.clone(),
                        });
                    }
                }
                for (path, (_, source, _)) in &cumulative {
                    overlays.push(SourceOverlay::Replace {
                        path: path.clone(),
                        language: SourceLanguage::Java,
                        source: source.clone(),
                    });
                }
                let current_index = index.with_overlays(&overlays)?;
                let planned = if let Some(seed) = &retained_seed {
                    transpiler::fixpoint::plan_workspace_warm(
                        &virtual_sources,
                        cli,
                        &current_index,
                        &translation_roots,
                        cli.max_retention_passes,
                        false,
                        seed,
                    )?
                } else {
                    transpiler::fixpoint::plan_workspace_state(
                        &virtual_sources,
                        cli,
                        &current_index,
                        &translation_roots,
                        cli.max_retention_passes,
                        false,
                    )?
                };
                retained_seed = Some(planned.roots);
                let plans = planned.plans;
                let mut next = virtual_sources.clone();
                let cumulative_before = cumulative.clone();
                let strict = matches!(cli.untranslatable, UntranslatableMode::Error);
                for plan in &plans {
                    let blocked = strict && (plan.errors > 0 || plan.warnings > 0);
                    if blocked {
                        continue;
                    }
                    match migrate::propose_speculative_migration(&plan.source, &plan.coverage) {
                        migrate::MigrationProposal::Untouched => {}
                        migrate::MigrationProposal::Delete => {
                            next.retain(|(p, _)| p != &plan.file);
                        }
                        migrate::MigrationProposal::Rewrite(text) => {
                            if let Some(item) = next.iter_mut().find(|(p, _)| p == &plan.file) {
                                item.1 = text;
                            }
                        }
                    }
                    for (name, content) in &plan.java_files {
                        let target = plan.file.parent().unwrap_or(Path::new(".")).join(name);
                        let key = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
                        if let Some((origin, old, _)) = cumulative.get(&key)
                            && origin != &plan.file
                            && old != content
                        {
                            return Err(format!(
                                "generated Java path conflict: {}",
                                notlin::paths::display(&target)
                            ));
                        }
                        cumulative.insert(key, (plan.file.clone(), content.clone(), name.clone()));
                    }
                }
                if profile_speculation {
                    log_speculative_changes(
                        round,
                        &virtual_sources,
                        &next,
                        &cumulative_before,
                        &cumulative,
                        cli.verbose > 0,
                    );
                }
                progress_activity(
                    "plan",
                    format!(
                        "pass {round} · {} · {} ready",
                        item_count(next.len(), "Kotlin source remains", "Kotlin sources remain"),
                        item_count(cumulative.len(), "Java output", "Java outputs")
                    ),
                );
                if next == virtual_sources && cumulative == cumulative_before {
                    converged_round = round;
                    converged_plans = Some(plans);
                    break;
                }
                virtual_sources = next;
            }
            let Some(final_plans) = converged_plans else {
                return Err(format!(
                    "workspace migration did not converge within {limit} speculative rounds"
                ));
            };
            progress_done(
                "plan",
                format!(
                    "converged in {} · {}",
                    item_count(converged_round, "pass", "passes"),
                    item_count(cumulative.len(), "Java output", "Java outputs")
                ),
            );
            progress_start("write", "applying the converged migration");
            let mut final_map: HashMap<_, _> = virtual_sources.iter().cloned().collect();
            for plan in &final_plans {
                match migrate::propose_migration(&plan.source, &plan.coverage) {
                    migrate::MigrationProposal::Untouched => {}
                    migrate::MigrationProposal::Delete => {
                        final_map.remove(&plan.file);
                    }
                    migrate::MigrationProposal::Rewrite(text) => {
                        final_map.insert(plan.file.clone(), text);
                    }
                }
            }
            for plan in &final_plans {
                log::debug!(
                    "planned {}: {} Java output(s), {} declaration(s) translated, {} retained",
                    notlin::paths::display(&plan.file),
                    plan.java_files.len(),
                    plan.coverage.translated.len(),
                    plan.coverage.untranslated.len()
                );
                total_errors += plan.errors;
                total_warnings += plan.warnings;
            }
            let mut emitted_by_origin: HashMap<PathBuf, HashSet<String>> = HashMap::new();
            for (origin, _, name) in cumulative.values() {
                emitted_by_origin
                    .entry(origin.clone())
                    .or_default()
                    .insert(name.clone());
            }
            for (path, (origin, content, name)) in &cumulative {
                let _ = path;
                log::debug!(
                    "planned {}: Java output {name}",
                    notlin::paths::display(origin)
                );
                write_java_files(cli, origin, &[(name.clone(), content.clone())], &mut stdout)?;
                java_written += 1;
            }
            if cli.out_dir.is_none() {
                for (path, _) in &sources {
                    remove_stale_generated_outputs(
                        path,
                        emitted_by_origin.get(path).cloned().unwrap_or_default(),
                    )?;
                }
            }
            for (path, source) in &sources {
                let proposal = match final_map.get(path) {
                    None => migrate::MigrationProposal::Delete,
                    Some(text) if text == source => migrate::MigrationProposal::Untouched,
                    Some(text) => migrate::MigrationProposal::Rewrite(text.clone()),
                };
                let outcome = migrate::apply_migration_proposal(path, &proposal)?;
                outcomes.push((path.clone(), outcome));
            }
            // Migration writes Java after the initial Kotlin/Java index was
            // built. Re-index once before repairing retained Kotlin so its
            // Java getter boundaries resolve against the generated sources,
            // not their pre-migration Kotlin declarations.
            progress_done(
                "write",
                format!(
                    "{} · {} migrated",
                    item_count(java_written, "Java file", "Java files"),
                    item_count(outcomes.len(), "source", "sources")
                ),
            );
            progress_start("repair", "checking residual Kotlin boundaries");
            let migrated_index = SourceIndex::discover(&workspace_root)?;
            let manual_marks = outcomes
                .iter()
                .map(|(file, outcome)| annotate_manual_marks(file, outcome, &migrated_index))
                .sum::<usize>();
            progress_done(
                "repair",
                format!(
                    "{} flagged",
                    item_count(manual_marks, "manual spot", "manual spots")
                ),
            );
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

    progress_start(
        "translate",
        format!(
            "processing {}",
            item_count(files.len(), "source", "sources")
        ),
    );
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
                        .map_err(|e| format!("{}: {e}", notlin::paths::display(&target)))?;
                    let mut writer = BufWriter::new(file);
                    writer
                        .write_all(content.as_bytes())
                        .map_err(|e| format!("{}: {e}", notlin::paths::display(&target)))?;
                    writer
                        .flush()
                        .map_err(|e| format!("{}: {e}", notlin::paths::display(&target)))?;
                    log::debug!("wrote {}", notlin::paths::display(&target));
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
                        log::debug!(
                            "{}: no translated content; untouched",
                            notlin::paths::display(file)
                        );
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
                                        log::info!(
                                            "deleted orphan {}",
                                            notlin::paths::display(&path)
                                        );
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

    progress_done(
        "translate",
        format!(
            "{} ready",
            item_count(java_written, "Java output", "Java outputs")
        ),
    );
    finish_run(
        cli,
        files.len(),
        total_errors,
        total_warnings,
        java_written,
        &outcomes,
        &mut stdout,
    )
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
                    .map_err(|e| format!("{}: {e}", notlin::paths::display(&target)))?;
                let mut writer = BufWriter::new(out);
                writer
                    .write_all(content.as_bytes())
                    .map_err(|e| format!("{}: {e}", notlin::paths::display(&target)))?;
                writer
                    .flush()
                    .map_err(|e| format!("{}: {e}", notlin::paths::display(&target)))?;
                log::debug!("wrote {}", notlin::paths::display(&target));
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

fn log_speculative_changes(
    round: usize,
    before_kotlin: &[(PathBuf, String)],
    after_kotlin: &[(PathBuf, String)],
    before_java: &HashMap<PathBuf, (PathBuf, String, String)>,
    after_java: &HashMap<PathBuf, (PathBuf, String, String)>,
    verbose: bool,
) {
    let before_kotlin: HashMap<_, _> = before_kotlin
        .iter()
        .map(|(path, source)| (path, source.as_bytes()))
        .collect();
    let after_kotlin: HashMap<_, _> = after_kotlin
        .iter()
        .map(|(path, source)| (path, source.as_bytes()))
        .collect();
    let mut kotlin_added = Vec::new();
    let mut kotlin_removed = Vec::new();
    let mut kotlin_changed = Vec::new();
    for (path, source) in &after_kotlin {
        match before_kotlin.get(path) {
            None => kotlin_added.push((*path).clone()),
            Some(old) if *old != *source => kotlin_changed.push((*path).clone()),
            Some(_) => {}
        }
    }
    for path in before_kotlin.keys() {
        if !after_kotlin.contains_key(path) {
            kotlin_removed.push((*path).clone());
        }
    }
    let mut java_added = Vec::new();
    let mut java_removed = Vec::new();
    let mut java_changed = Vec::new();
    for (path, (_, content, _)) in after_java {
        match before_java.get(path) {
            None => java_added.push(path.clone()),
            Some((_, old, _)) if old != content => java_changed.push(path.clone()),
            Some(_) => {}
        }
    }
    for path in before_java.keys() {
        if !after_java.contains_key(path) {
            java_removed.push(path.clone());
        }
    }
    eprintln!(
        "NOTLIN_PROFILE speculative round {round}: Kotlin added={} removed={} byte-changed={}; cumulative Java added={} removed={} content-changed={}",
        kotlin_added.len(),
        kotlin_removed.len(),
        kotlin_changed.len(),
        java_added.len(),
        java_removed.len(),
        java_changed.len()
    );
    if !verbose {
        return;
    }
    for (category, mut paths) in [
        ("Kotlin added", kotlin_added),
        ("Kotlin removed", kotlin_removed),
        ("Kotlin byte-changed", kotlin_changed),
        ("Java added", java_added),
        ("Java removed", java_removed),
        ("Java content-changed", java_changed),
    ] {
        paths.sort();
        let samples = paths
            .iter()
            .take(20)
            .map(|path| notlin::paths::display(path).to_string())
            .collect::<Vec<_>>();
        eprintln!(
            "NOTLIN_PROFILE {category} samples: [{}]",
            samples.join(", ")
        );
    }
}

fn remove_stale_generated_outputs(
    source: &Path,
    emitted_names: HashSet<String>,
) -> Result<(), String> {
    let Some(directory) = source.parent() else {
        return Ok(());
    };
    let source_canon = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    let source_text = source_canon.to_string_lossy().replace('\\', "/");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if path.extension().and_then(|extension| extension.to_str()) != Some("java")
            || emitted_names.contains(name)
        {
            continue;
        }
        let generated_from_source = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| content.lines().next().map(str::to_string))
            .is_some_and(|first_line| {
                first_line.contains("NOTLIN: generated from") && first_line.contains(&source_text)
            });
        if generated_from_source {
            std::fs::remove_file(&path)
                .map_err(|error| format!("{}: {error}", notlin::paths::display(&path)))?;
            log::info!(
                "deleted stale generated output {}",
                notlin::paths::display(&path)
            );
        }
    }
    Ok(())
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
    stdout.flush().map_err(|error| format!("stdout: {error}"))?;
    print_retention_report();
    print_run_summary(
        file_count,
        total_errors,
        total_warnings,
        java_written,
        outcomes,
        failed,
    );
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
        let input = std::fs::File::open(file)
            .map_err(|e| format!("{}: {e}", notlin::paths::display(file)))?;
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
        let entries = std::fs::read_dir(&directory)
            .map_err(|e| format!("{}: {e}", notlin::paths::display(&directory)))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("{}: {e}", notlin::paths::display(&directory)))?;
            let path = entry.path();
            let is_kotlin = path.extension().is_some_and(|extension| extension == "kt");
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) if !is_kotlin => continue,
                Err(e) => return Err(format!("{}: {e}", notlin::paths::display(&path))),
            };
            if metadata.is_dir() {
                let canonical = std::fs::canonicalize(&path)
                    .map_err(|e| format!("{}: {e}", notlin::paths::display(&path)))?;
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
