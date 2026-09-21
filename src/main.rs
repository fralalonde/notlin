use clap::Parser as _;
use colored::Colorize;
use notlin::cli::{Cli, UntranslatableMode};
use notlin::transpiler;
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

        let (java_files, errors, warnings) = transpiler::transpile(&source, file, cli);
        total_errors += errors;
        total_warnings += warnings;

        match &cli.out_dir {
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
