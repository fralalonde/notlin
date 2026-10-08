//! Run with `cargo run --example validate_mixed -- --help`.
use clap::Parser;
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    about = "Compile remaining Kotlin and generated Java together in a fresh output directory"
)]
struct Args {
    #[arg(long)]
    kotlinc: PathBuf,
    #[arg(long)]
    javac: PathBuf,
    #[arg(long)]
    java: PathBuf,
    #[arg(long)]
    out: PathBuf,
    /// Dependency directory or JAR; repeat for multiple entries, including Kotlin stdlib.
    #[arg(long)]
    classpath: Vec<PathBuf>,
    /// Optional fully qualified JVM main class to run after compiling.
    #[arg(long)]
    main: Option<String>,
    /// Explicit .kt and .java source files forming one compilation unit.
    #[arg(required = true)]
    sources: Vec<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let mut config = JvmValidationConfig::new(args.kotlinc, args.javac, args.java, args.out);
    config.classpath = args.classpath;
    config.run_main_class = args.main;
    for source in args.sources {
        match source.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(source),
            Some("java") => config.java_sources.push(source),
            _ => {
                eprintln!("expected a .kt or .java source: {}", source.display());
                return ExitCode::FAILURE;
            }
        }
    }
    match validate_jvm_sources(&config) {
        Ok(report) => {
            for command in report.commands {
                println!("{} passed", command.stage);
                let diagnostics = command.diagnostics();
                if !diagnostics.is_empty() {
                    println!("{diagnostics}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
