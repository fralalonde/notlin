use clap::Parser as _;
use colored::Colorize;
use notlin::cli::{Cli, JvmFactsMode, UntranslatableMode};
use notlin::migrate::{self, MigrateOutcome};
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

static PROGRESS_OUTPUT: OnceLock<Mutex<bool>> = OnceLock::new();
static LAUNCH_STARTED: OnceLock<Instant> = OnceLock::new();
static VERBOSE_TIMING: OnceLock<bool> = OnceLock::new();

fn elapsed_log_label(elapsed: Duration) -> String {
    format!(
        "[+{:02}:{:02}.{:03}]",
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60,
        elapsed.subsec_millis()
    )
}

fn verbose_elapsed_prefix() -> String {
    if VERBOSE_TIMING.get().copied().unwrap_or(false) {
        format!(
            "{} ",
            elapsed_log_label(LAUNCH_STARTED.get_or_init(Instant::now).elapsed())
        )
    } else {
        String::new()
    }
}

fn finish_active_line(stderr: &mut impl Write, active: &mut bool) {
    if *active {
        // Remove the trailing spinner while preserving the activity text.
        let _ = writeln!(stderr, "\x08 \x08");
        *active = false;
    }
}

fn progress_line(marker: &str, phase: &str, detail: impl std::fmt::Display) {
    let elapsed = verbose_elapsed_prefix();
    let label = format!("{phase:<11}");
    let mut active = PROGRESS_OUTPUT
        .get_or_init(|| Mutex::new(false))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut stderr = std::io::stderr().lock();
    finish_active_line(&mut stderr, &mut active);
    let _ = writeln!(
        stderr,
        "  {marker} {elapsed}{}{}",
        label.bold(),
        detail.to_string().bright_black()
    );
}

fn progress_active(phase: &str, detail: &str, spinner: char) {
    let elapsed = verbose_elapsed_prefix();
    let label = format!("{phase:<11}");
    let mut active = PROGRESS_OUTPUT
        .get_or_init(|| Mutex::new(false))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut stderr = std::io::stderr().lock();
    finish_active_line(&mut stderr, &mut active);
    let _ = write!(
        stderr,
        "  {} {elapsed}{}{} {spinner}",
        "◇".cyan(),
        label.bold(),
        detail.bright_black()
    );
    *active = true;
    let _ = stderr.flush();
}

fn progress_spinner(spinner: char) {
    let active = PROGRESS_OUTPUT
        .get_or_init(|| Mutex::new(false))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !*active {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    let _ = write!(stderr, "\x08{spinner}");
    let _ = stderr.flush();
}

fn progress_start(phase: &str, detail: impl std::fmt::Display) {
    progress_line(&"◇".cyan().to_string(), phase, detail);
}

fn progress_activity(phase: &str, detail: impl std::fmt::Display) {
    progress_line(&"·".bright_black().to_string(), phase, detail);
}

fn progress_done(phase: &str, detail: impl std::fmt::Display) {
    progress_line(&"✓".green().to_string(), phase, detail);
}

fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

fn spinner_frame(tick: usize) -> char {
    ['|', '/', '-', '\\'][tick % 4]
}

fn run_result_text(failed: bool, elapsed: Duration) -> String {
    format!(
        "{} · elapsed {}",
        if failed { "failed" } else { "success" },
        elapsed_label(elapsed)
    )
}

fn kotlin_scope_counts(declarations: usize, files: usize) -> String {
    format!(
        "{} ({})",
        item_count(declarations, "declaration", "declarations"),
        item_count(files, "Kotlin file", "Kotlin files")
    )
}

fn indexed_input_declarations(index: &SourceIndex, files: &[PathBuf]) -> Option<usize> {
    files.iter().try_fold(0usize, |total, path| {
        let file = index.source_file(path).or_else(|| {
            let canonical = std::fs::canonicalize(path).ok()?;
            index.source_file(&canonical)
        })?;
        Some(total + file.kotlin_declaration_count())
    })
}

struct ProgressReporter {
    started: Instant,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
    phase: String,
}

impl ProgressReporter {
    fn start(phase: &str, detail: impl std::fmt::Display) -> Self {
        let started = Instant::now();
        let detail = detail.to_string();
        let phase = phase.to_owned();
        let is_terminal = std::io::stderr().is_terminal();
        if is_terminal {
            progress_active(&phase, &detail, spinner_frame(0));
        } else {
            progress_start(&phase, &detail);
        }

        let (stop, heartbeat) = if is_terminal {
            let (stop, stopped) = mpsc::channel();
            let heartbeat = std::thread::spawn(move || {
                let mut tick = 1usize;
                loop {
                    match stopped.recv_timeout(Duration::from_millis(120)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            progress_spinner(spinner_frame(tick));
                            tick += 1;
                        }
                    }
                }
            });
            (Some(stop), Some(heartbeat))
        } else {
            (None, None)
        };

        Self {
            started,
            stop,
            heartbeat,
            phase,
        }
    }

    fn update(&self, detail: impl std::fmt::Display) {
        let detail = detail.to_string();
        if std::io::stderr().is_terminal() {
            progress_active(&self.phase, &detail, spinner_frame(0));
        } else {
            progress_activity(&self.phase, detail);
        }
    }

    fn finish(mut self, detail: impl std::fmt::Display) {
        self.stop_heartbeat();
        let detail = format!(
            "{detail} · elapsed {}",
            elapsed_label(self.started.elapsed())
        );
        progress_done(&self.phase, detail);
    }

    fn stop_heartbeat(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.stop_heartbeat();
        let mut active = PROGRESS_OUTPUT
            .get_or_init(|| Mutex::new(false))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        finish_active_line(&mut std::io::stderr().lock(), &mut active);
    }
}

#[cfg(test)]
mod progress_tests {
    use super::elapsed_label;
    use std::time::Duration;

    #[test]
    fn elapsed_label_formats_seconds_and_minutes() {
        assert_eq!(elapsed_label(Duration::from_secs(9)), "9s");
        assert_eq!(elapsed_label(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn verbose_timestamps_include_minutes_seconds_and_milliseconds() {
        assert_eq!(
            super::elapsed_log_label(Duration::from_millis(125678)),
            "[+02:05.678]"
        );
        assert_eq!(
            super::elapsed_log_label(Duration::from_millis(9)),
            "[+00:00.009]"
        );
    }

    #[test]
    fn spinner_cycles_through_four_frames() {
        assert_eq!(
            (0..6).map(super::spinner_frame).collect::<String>(),
            "|/-\\|/"
        );
    }

    #[test]
    fn run_result_includes_total_elapsed_time() {
        assert_eq!(
            super::run_result_text(false, Duration::from_secs(125)),
            "success · elapsed 2m 05s"
        );
        assert_eq!(
            super::run_result_text(true, Duration::from_secs(9)),
            "failed · elapsed 9s"
        );
    }

    #[test]
    fn kotlin_counts_put_declarations_before_files() {
        assert_eq!(
            super::kotlin_scope_counts(9, 2),
            "9 declarations (2 Kotlin files)"
        );
        assert_eq!(
            super::kotlin_scope_counts(1, 1),
            "1 declaration (1 Kotlin file)"
        );
    }
}

fn item_count(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn print_run_summary(
    counts: &RunCounts,
    outcomes: &[(PathBuf, MigrateOutcome)],
    failed: bool,
    run_elapsed: Duration,
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
        "Kotlin".bold(),
        format!(
            "{} processed",
            kotlin_scope_counts(counts.declarations, counts.sources)
        )
        .bright_black()
    );
    eprintln!(
        "  {} {:<11} {}",
        "├─".bright_black(),
        "java".bold(),
        format!("{} written", counts.java_written).bright_black()
    );
    if let Some(coverage) = &counts.coverage {
        eprintln!(
            "  {} {:<11} {}",
            "├─".bright_black(),
            "coverage".bold(),
            coverage.summary().bright_black()
        );
    }
    if !outcomes.is_empty() {
        eprintln!(
            "  {} {:<11} {}",
            "├─".bright_black(),
            "migration".bold(),
            format!("{deleted} files deleted · {trimmed} trimmed · {untouched} untouched")
                .bright_black()
        );
    }
    eprintln!(
        "  {} {:<11} {}",
        "├─".bright_black(),
        "diagnostics".bold(),
        format!("{} errors · {} warnings", counts.errors, counts.warnings).bright_black()
    );
    eprintln!(
        "  {} {:<11} {}",
        "└─".bright_black(),
        "result".bold(),
        if failed {
            run_result_text(true, run_elapsed).red().bold()
        } else {
            run_result_text(false, run_elapsed).green().bold()
        }
    );
}

fn main() -> ExitCode {
    LAUNCH_STARTED.get_or_init(Instant::now);
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
    let _ = VERBOSE_TIMING.set(cli.verbose > 0);
    env_logger::Builder::new()
        .filter_level(match cli.verbose {
            0 => log::LevelFilter::Warn,
            1 => log::LevelFilter::Info,
            2 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        })
        .format(|buffer, record| {
            let mut active = PROGRESS_OUTPUT
                .get_or_init(|| Mutex::new(false))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            finish_active_line(&mut std::io::stderr().lock(), &mut active);
            let elapsed = verbose_elapsed_prefix();
            for line in record.args().to_string().lines() {
                writeln!(buffer, "  {} {elapsed}{line}", "│".bright_black())?;
            }
            Ok(())
        })
        .init();
    log::info!("cli: {:?}", cli);

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
fn annotate_manual_marks(
    file: &Path,
    outcome: &MigrateOutcome,
    index: &SourceIndex,
    property_abi_already_repaired: bool,
) -> usize {
    if matches!(outcome, MigrateOutcome::Deleted) {
        return 0;
    }
    if file.extension().and_then(|e| e.to_str()) != Some("kt") {
        return 0;
    }
    let marks = if property_abi_already_repaired {
        notlin::manual_marks::annotate_manual_spots_after_speculation(file, index)
    } else {
        notlin::manual_marks::annotate_manual_spots(file, index)
    };
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
    let run_started = Instant::now();
    // Fail before any workspace scan or write: `on` means the helper is not
    // optional, and its absence is a configuration error, not a silent fallback.
    if cli.jvm_facts == JvmFactsMode::On && notlin::facts::client::FactsClient::jar_path().is_none()
    {
        return Err("--jvm-facts=on requires NOTLIN_JVM_FACTS_JAR".into());
    }
    if let Some(path) = &cli.coverage_report {
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            return Err("--coverage-report requires a .json output path".into());
        }
        if cli.dump_ast {
            return Err("--coverage-report cannot be combined with --dump-ast".into());
        }
        if let (Ok(report), Some(config)) = (std::fs::canonicalize(path), &cli.validation_config)
            && std::fs::canonicalize(config).is_ok_and(|config| config == report)
        {
            return Err("coverage report must not overwrite the validation configuration".into());
        }
    }
    if cli.dump_ast {
        let files = collect_inputs(&cli.input)?;
        if files.is_empty() {
            return Err("no input files given for AST inspection".into());
        }
        let mut stdout = BufWriter::new(std::io::stdout().lock());
        for file in files {
            stdout
                .write_all(transpiler::dump_ast(&read_source(&file)?).as_bytes())
                .map_err(|error| format!("stdout: {error}"))?;
        }
        stdout.flush().map_err(|error| format!("stdout: {error}"))?;
        return Ok(ExitCode::SUCCESS);
    }
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
            kotlin_scope_counts(
                index
                    .kotlin_files()
                    .map(|file| file.kotlin_declaration_count())
                    .sum(),
                index.kotlin_files().count(),
            ),
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
    // Track B facts are indexed per *module*: the helper needs the run's Kotlin
    // source roots, and the same classpath the Java side validates against. Both
    // are child-process configuration, applied before any provider exists (the
    // fixpoint provider is what installs the helper). `--jvm-facts=off` stays a
    // hard no-op here: no configuration means no reason to touch anything.
    if cli.jvm_facts != JvmFactsMode::Off && files.iter().all(|file| file.as_os_str() != "-") {
        notlin::facts::runtime::configure_kotlin_sources(files.iter().cloned());
        if let Some(config_path) = &cli.validation_config {
            match notlin::jvm_validation::validation_classpath(config_path) {
                Ok(classpath) => notlin::facts::runtime::configure_classpath(classpath),
                Err(error) => log::debug!(
                    "validation classpath unavailable for the JVM facts helper: {error}"
                ),
            }
        }
    }
    let selected_declarations = indexed_input_declarations(&index, &files);
    progress_done(
        "input",
        format!(
            "{} selected",
            selected_declarations
                .map(|count| kotlin_scope_counts(count, files.len()))
                .unwrap_or_else(|| item_count(files.len(), "input", "inputs"))
        ),
    );
    let stdout = std::io::stdout();
    let mut stdout = BufWriter::new(stdout.lock());
    if files.is_empty() {
        if !cli.input.is_empty() && cli.input.iter().all(|input| input.is_dir()) {
            return finish_run(cli, RunCounts::default(), &[], &mut stdout, run_started);
        }
        return Err("no input files given (positional <INPUT>..., or use - for stdin)".into());
    }

    if cli.validation_config.is_some() && files.iter().any(|f| f.as_os_str() == "-") {
        return Err(
            "--validation-config requires file-backed inputs; stdin cannot be staged".into(),
        );
    }

    let mut total_errors = 0usize;
    let mut total_warnings = 0usize;
    let mut java_written = 0usize;
    let mut outcomes: Vec<(PathBuf, MigrateOutcome)> = Vec::new();

    // Workspace mode (indexed inputs): compute the retention fixpoint first
    // (probe passes in memory, no writes, no printed diagnostics), then write
    // each file's plan exactly once. The subtype retention rule then only
    // fires for subtypes that are THEMSELVES retained, so clean
    // hub-and-implementor families translate together. This is the only path
    // whose subtype and supertype rules are paired: the legacy hintless path
    // retains a supertype conservatively but has no upward rule, so a Java
    // child could be emitted against a Kotlin-retained parent. Every
    // file-backed run — including a single file — goes through the fixpoint;
    // stdin has no indexable path and keeps the legacy single-file path.
    if !cli.dump_ast {
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
            let plan_progress = ProgressReporter::start(
                "plan",
                format!(
                    "resolving {}",
                    indexed_input_declarations(&index, &files)
                        .map(|count| kotlin_scope_counts(count, files.len()))
                        .unwrap_or_else(|| item_count(files.len(), "Kotlin file", "Kotlin files"))
                ),
            );
            let plan = notlin::migration_pipeline::plan_workspace_migration(
                sources,
                cli,
                &index,
                &translation_roots,
                |detail| plan_progress.update(detail),
            )?;
            let declaration_count = plan
                .translation_plans
                .values()
                .map(|plan| plan.declarations.len())
                .sum();
            let coverage = notlin::coverage::CoverageReport::collect(
                plan.translation_plans.values(),
                plan.generated_java
                    .iter()
                    .map(|output| (output.path.as_path(), output.owners.as_slice())),
            )?;
            plan_progress.finish(format!(
                "converged in {} · {}",
                item_count(plan.rounds, "pass", "passes"),
                item_count(plan.generated_java.len(), "Java output", "Java outputs")
            ));
            if cli.advice {
                let advice_started = Instant::now();
                let advice_progress =
                    ProgressReporter::start("advice", "checking targeted source recommendations");
                let advice = notlin::translation_advice::analyze(
                    &plan.final_plans,
                    &plan.translation_plans,
                    &index,
                    &translation_roots,
                    &notlin::diagnostics::retention_sites(),
                );
                advice_progress.finish(item_count(
                    advice.len(),
                    "recommendation",
                    "recommendations",
                ));
                if std::env::var_os("NOTLIN_PROFILE").is_some() {
                    eprintln!(
                        "NOTLIN_PROFILE advice: {:.3}s",
                        advice_started.elapsed().as_secs_f64()
                    );
                }
                eprint!("{}", notlin::translation_advice::render(&advice, 10));
            }
            if let Some(config_path) = &cli.validation_config {
                notlin::migration_pipeline::validate_workspace_plan(
                    &plan,
                    &index
                        .kotlin_files()
                        .map(|file| file.path.clone())
                        .collect::<Vec<_>>(),
                    &index
                        .java_files()
                        .map(|file| file.path.clone())
                        .collect::<Vec<_>>(),
                    config_path,
                )?;
            }
            notlin::migration_pipeline::verify_original_snapshots(&plan)?;
            let apply_migration = cli.migrates_in_place();
            progress_start(
                "write",
                if apply_migration {
                    "applying the converged migration"
                } else {
                    "writing accepted Java outputs"
                },
            );
            let final_plans = &plan.final_plans;
            for file_plan in final_plans {
                log::debug!(
                    "planned {}: {} Java output(s), {} declaration(s) translated, {} retained",
                    notlin::paths::display(&file_plan.file),
                    file_plan.java_files.len(),
                    file_plan.coverage.translated.len(),
                    file_plan.coverage.untranslated.len()
                );
                total_errors += file_plan.errors;
                total_warnings += file_plan.warnings;
            }
            let mut emitted_by_origin: HashMap<PathBuf, HashSet<String>> = HashMap::new();
            for generated in &plan.generated_java {
                emitted_by_origin
                    .entry(generated.origin.clone())
                    .or_default()
                    .insert(generated.name.clone());
            }
            for generated in &plan.generated_java {
                log::debug!(
                    "planned {}: Java output {}",
                    notlin::paths::display(&generated.origin),
                    generated.name
                );
                write_java_files(
                    cli,
                    &generated.origin,
                    &[(generated.name.clone(), generated.source.clone())],
                    &mut stdout,
                )?;
                java_written += 1;
            }
            if apply_migration && cli.out_dir.is_none() {
                for (path, _) in &plan.original_sources {
                    remove_stale_generated_outputs(
                        path,
                        emitted_by_origin.get(path).cloned().unwrap_or_default(),
                        plan.translation_plans
                            .get(path)
                            .map(retained_java_type_names)
                            .unwrap_or_default(),
                    )?;
                }
            }
            if apply_migration {
                for (path, _) in &plan.original_sources {
                    let proposal = plan.migration_proposals.get(path).ok_or_else(|| {
                        format!(
                            "migration plan missing source proposal: {}",
                            notlin::paths::display(path)
                        )
                    })?;
                    outcomes.push((
                        path.clone(),
                        migrate::apply_migration_proposal(path, proposal)?,
                    ));
                }
            }
            // Migration writes Java after the initial Kotlin/Java index was
            // built. Re-index once before annotating retained Kotlin so links
            // and blocker locations resolve against the final workspace.
            progress_done(
                "write",
                format!(
                    "{} processed · {}",
                    kotlin_scope_counts(declaration_count, files.len()),
                    item_count(java_written, "Java file", "Java files")
                ),
            );
            let repair_progress =
                ProgressReporter::start("annotate", "refreshing retained Kotlin explanations");
            let migrated_index = SourceIndex::discover(&workspace_root)?;
            let manual_marks = outcomes
                .iter()
                .map(|(file, outcome)| annotate_manual_marks(file, outcome, &migrated_index, true))
                .sum::<usize>();
            repair_progress.finish(format!(
                "{} flagged",
                item_count(manual_marks, "manual spot", "manual spots")
            ));
            return finish_run(
                cli,
                RunCounts {
                    sources: files.len(),
                    declarations: declaration_count,
                    errors: total_errors,
                    warnings: total_warnings,
                    java_written,
                    coverage: Some(coverage),
                },
                &outcomes,
                &mut stdout,
                run_started,
            );
        }
    }

    let mut declaration_count = 0usize;
    let mut coverage_plans = Vec::new();
    let mut coverage_outputs = Vec::new();
    let mut parsed_files = 0usize;
    progress_start(
        "translate",
        format!(
            "processing {}",
            selected_declarations
                .map(|count| kotlin_scope_counts(count, files.len()))
                .unwrap_or_else(|| item_count(files.len(), "input", "inputs"))
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

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .map_err(|error| error.to_string())?;
        let tree = parser.parse(&source, None).ok_or("Kotlin parsing failed")?;
        declaration_count +=
            notlin::translation_plan::ownership_boundary_count(&source, tree.root_node());
        parsed_files += 1;
        progress_activity(
            "translate",
            format!(
                "{} parsed",
                kotlin_scope_counts(declaration_count, parsed_files)
            ),
        );
        let planned = transpiler::plan_with_tree_hint(
            &source,
            &tree,
            file,
            cli,
            transpiler::WorkspaceScope {
                index: Some(&index),
                roots: &translation_roots,
                ..Default::default()
            },
            true,
        );
        if planned.emission_failed {
            return Err(format!(
                "{}: accepted Java emission failed",
                notlin::paths::display(file)
            ));
        }
        coverage_outputs.extend(planned.emitted_file_owners.iter().map(|(name, owners)| {
            (
                cli.out_dir
                    .as_deref()
                    .or_else(|| file.parent())
                    .unwrap_or(Path::new("."))
                    .join(name),
                owners.clone(),
            )
        }));
        let retained_java_types = retained_java_type_names(&planned.plan);
        coverage_plans.push(planned.plan);
        let java_files = planned.accepted.emit();
        let (errors, warnings, coverage) = (planned.errors, planned.warnings, planned.coverage);
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
                    }
                }
                // Clean stale outputs after any successful in-place migration,
                // including a trim that translated other declarations from
                // this file. Preserve outputs for absent Kotlin owners.
                if effective_out_dir.is_none() {
                    remove_stale_generated_outputs(
                        file,
                        java_files.iter().map(|(name, _)| name.clone()).collect(),
                        retained_java_types.clone(),
                    )?;
                }
                annotate_manual_marks(file, &outcome, &index, false);
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
        RunCounts {
            sources: files.len(),
            declarations: declaration_count,
            errors: total_errors,
            warnings: total_warnings,
            java_written,
            coverage: if cli.dump_ast {
                None
            } else {
                Some(notlin::coverage::CoverageReport::collect(
                    &coverage_plans,
                    coverage_outputs
                        .iter()
                        .map(|(path, owners)| (path.as_path(), owners.as_slice())),
                )?)
            },
        },
        &outcomes,
        &mut stdout,
        run_started,
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

fn remove_stale_generated_outputs(
    source: &Path,
    emitted_names: HashSet<String>,
    retained_java_types: HashSet<String>,
) -> Result<(), String> {
    let Some(directory) = source.parent() else {
        return Ok(());
    };
    let source_canon = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    let source_text = source_canon.to_string_lossy().replace('\\', "/");
    let source_display = notlin::paths::display(source);
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
        let generated_from_source = std::fs::read_to_string(&path).ok().is_some_and(|content| {
            let type_name = path.file_stem().and_then(|stem| stem.to_str());
            let header_matches = content.lines().next().is_some_and(|first_line| {
                let Some((_, marker)) = first_line.split_once("NOTLIN: generated from") else {
                    return false;
                };
                let normalize = |path: &str| {
                    let path = path.trim().replace('\\', "/");
                    if cfg!(windows) {
                        path.to_ascii_lowercase()
                    } else {
                        path
                    }
                };
                let marker = normalize(marker);
                [normalize(&source_text), normalize(&source_display)]
                    .iter()
                    .any(|candidate| {
                        marker == *candidate
                            || marker
                                .strip_prefix(candidate)
                                .is_some_and(|suffix| suffix.starts_with(" —"))
                    })
            });
            let declared_type = notlin::java_ir::parse_java(&content).ok().and_then(|unit| {
                let declarations = unit
                    .root
                    .children
                    .iter()
                    .filter_map(|element| match element {
                        notlin::java_ir::JavaElement::Declaration(node)
                            if matches!(
                                node.kind.as_str(),
                                "class_declaration"
                                    | "interface_declaration"
                                    | "enum_declaration"
                                    | "annotation_type_declaration"
                                    | "record_declaration"
                            ) =>
                        {
                            Some(node)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                (declarations.len() == 1)
                    .then(|| java_declared_name(declarations[0]))
                    .flatten()
            });
            header_matches
                && type_name.is_some_and(|name| {
                    retained_java_types.contains(name) && declared_type.as_deref() == Some(name)
                })
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

fn java_declared_name(node: &notlin::java_ir::JavaSyntaxNode) -> Option<String> {
    use notlin::java_ir::JavaElement;
    fn find(node: &notlin::java_ir::JavaSyntaxNode) -> Option<String> {
        for child in &node.children {
            match child {
                JavaElement::Token(token) if token.field_name.as_deref() == Some("name") => {
                    return Some(token.text.clone());
                }
                JavaElement::Declaration(n)
                | JavaElement::Type(n)
                | JavaElement::Expression(n)
                | JavaElement::Statement(n)
                | JavaElement::Annotation(n)
                | JavaElement::Import(n)
                | JavaElement::Other(n)
                    if n.field_name.as_deref() == Some("name") =>
                {
                    return n.children.iter().find_map(|child| match child {
                        JavaElement::Token(token) => Some(token.text.clone()),
                        _ => None,
                    });
                }
                JavaElement::Declaration(n)
                | JavaElement::Type(n)
                | JavaElement::Expression(n)
                | JavaElement::Statement(n)
                | JavaElement::Annotation(n)
                | JavaElement::Import(n)
                | JavaElement::Other(n) => {
                    if let Some(name) = find(n) {
                        return Some(name);
                    }
                }
                _ => {}
            }
        }
        None
    }
    find(node)
}

fn retained_java_type_names(plan: &notlin::translation_plan::TranslationPlan) -> HashSet<String> {
    plan.declarations
        .iter()
        .filter(|decision| {
            decision.final_owner == Some(notlin::translation_plan::BackendOwner::Kotlin)
                && decision.symbol_id.owner_path.is_empty()
                && matches!(
                    decision.symbol_id.kind.as_str(),
                    "class" | "object" | "interface" | "enum" | "annotation"
                )
        })
        .map(|decision| decision.symbol_id.name.clone())
        .collect()
}

/// Shared run summary: failure policy, migration tally, final line.
#[derive(Default)]
struct RunCounts {
    sources: usize,
    declarations: usize,
    errors: usize,
    warnings: usize,
    java_written: usize,
    coverage: Option<notlin::coverage::CoverageReport>,
}

fn finish_run(
    cli: &Cli,
    counts: RunCounts,
    outcomes: &[(PathBuf, MigrateOutcome)],
    stdout: &mut BufWriter<std::io::StdoutLock<'_>>,
    run_started: Instant,
) -> Result<ExitCode, String> {
    let untranslatable_strict = matches!(cli.untranslatable, UntranslatableMode::Error);
    let failed = counts.errors > 0
        || (untranslatable_strict && counts.warnings > 0)
        || (cli.deny_warnings && counts.warnings > 0);
    stdout.flush().map_err(|error| format!("stdout: {error}"))?;
    if let Some(path) = &cli.coverage_report {
        let coverage = counts
            .coverage
            .as_ref()
            .ok_or("coverage is unavailable in --dump-ast mode")?;
        let json = serde_json::to_vec_pretty(coverage).map_err(|error| error.to_string())?;
        std::fs::write(path, json)
            .map_err(|error| format!("{}: {error}", notlin::paths::display(path)))?;
    }
    print_retention_report(counts.coverage.as_ref(), cli.verbose);
    print_run_summary(&counts, outcomes, failed, run_started.elapsed());
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Run-end retention table: what stayed Kotlin, why, how much of it, and where
/// a human has to decide. Part of the run summary, printed before exit.
fn print_retention_report(coverage: Option<&notlin::coverage::CoverageReport>, verbose: u8) {
    if let Some(coverage) = coverage.filter(|coverage| coverage.retained > 0) {
        eprint!("{}", coverage.retention_summary());
    }
    if verbose >= 2
        && let Some(report) = notlin::diagnostics::retention_report()
    {
        eprintln!(
            "\nnotlin: partial diagnostic-site hints; joined by names, not declaration coverage or unlock forecasts"
        );
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
                // A listed entry can vanish before it is opened (overlapping
                // migrator, concurrent cleanup); it has no content this run.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) if !is_kotlin => continue,
                Err(e) => return Err(format!("{}: {e}", notlin::paths::display(&path))),
            };
            if metadata.is_dir() {
                let canonical = match std::fs::canonicalize(&path) {
                    Ok(canonical) => canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(format!("{}: {e}", notlin::paths::display(&path))),
                };
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
