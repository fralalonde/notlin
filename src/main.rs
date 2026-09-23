use clap::Parser as _;
use colored::Colorize;
use notlin::cli::{Cli, UntranslatableMode};
use notlin::migrate;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::collections::HashSet;
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
    let index = SourceIndex::discover(&workspace_root)?;
    log::info!(
        "indexed {} Kotlin and {} Java files from {}",
        index.kotlin_files().count(),
        index.java_files().count(),
        workspace_root.display()
    );

    let translation_roots = if cli.input.is_empty() {
        vec![workspace_root.clone()]
    } else {
        cli.input
            .iter()
            .filter(|input| input.as_os_str() != "-")
            .map(|input| {
                log::info!("source root {}", input.to_string_lossy());
                std::fs::canonicalize(input)
                    .map_err(|e| format!("translation root {}: {e}", input.display()))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let files = collect_inputs(&cli.input)?;
    if files.is_empty() {
        return Err("no input files given (positional <INPUT>..., or use - for stdin)".into());
    }

    let mut total_errors = 0usize;
    let mut total_warnings = 0usize;

    for file in &files {
        log::info!("transpiling {}", file.display());
        let source = read_source(file)?;

        if cli.dump_ast {
            print!("{}", transpiler::dump_ast(&source));
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
                    std::fs::write(&target, content)
                        .map_err(|e| format!("{}: {e}", target.display()))?;
                    log::info!("wrote {}", target.display());
                }
            }
            None => {
                for (name, content) in &java_files {
                    if java_files.len() > 1 {
                        println!("// ===== {} =====", name);
                    }
                    print!("{content}");
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
                match migrate::migrate(file, &source, &coverage)? {
                    migrate::MigrateOutcome::Deleted => {
                        println!("{}: {}", "deleted".green(), file.display());
                    }
                    migrate::MigrateOutcome::Trimmed { remaining_bytes } => {
                        println!(
                            "{}: {} ({} bytes remain)",
                            "trimmed".yellow(),
                            file.display(),
                            remaining_bytes
                        );
                    }
                    migrate::MigrateOutcome::Untouched => {
                        log::info!("{}: no translated content; untouched", file.display());
                    }
                }
            } else {
                log::info!(
                    "{}: kept — run had errors/warnings in --untranslatable=error mode",
                    file.display()
                );
            }
        }
    }

    if cli.dump_ast {
        return Ok(ExitCode::SUCCESS);
    }

    let untranslatable_strict = matches!(cli.untranslatable, UntranslatableMode::Error);
    let failed = total_errors > 0
        || (untranslatable_strict && total_warnings > 0)
        || (cli.deny_warnings && total_warnings > 0);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn read_source(file: &Path) -> Result<String, String> {
    if file.as_os_str() == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("stdin: {e}"))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))
    }
}

fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for input in inputs {
        if input.as_os_str() == "-" {
            files.push(input.clone());
            continue;
        }
        if input.is_dir() {
            let mut dir_files: Vec<PathBuf> = walk(input)?
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "kt"))
                .collect();
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
    log::info!("collected {} .kt files", files.len());
    Ok(files)
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", d.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
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
