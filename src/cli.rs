use clap::Parser;
use std::path::PathBuf;

/// notlin — a Kotlin-to-Java transpiler.
#[derive(Parser, Debug)]
#[command(name = "notlin", version, about)]
pub struct Cli {
    /// .kt files or directories to transpile
    pub input: Vec<PathBuf>,

    /// Output directory for generated .java files (default: alongside input)
    #[arg(short, long)]
    pub out_dir: Option<PathBuf>,

    /// How to treat untranslatable constructs
    #[arg(long, value_enum, default_value = "warn")]
    pub untranslatable: UntranslatableMode,

    /// Exit non-zero if any warnings were emitted
    #[arg(long)]
    pub deny_warnings: bool,

    /// Nullability annotation set to emit
    #[arg(long, value_enum, default_value = "jetbrains")]
    pub annotations: Annotations,

    /// Print the tree-sitter parse tree instead of transpiling
    #[arg(long)]
    pub dump_ast: bool,

    /// Verbose logging (-v debug, -vv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum UntranslatableMode {
    /// Untranslatables are diagnostics-as-errors; transpilation fails
    Error,
    /// Untranslatables are warnings; best-effort output is still produced
    Warn,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Annotations {
    /// org.jetbrains.annotations @Nullable / @NotNull
    Jetbrains,
    /// org.jspecify.annotations Nullable / @NullMarked
    Jspecify,
    /// No nullability annotations emitted
    None,
}
