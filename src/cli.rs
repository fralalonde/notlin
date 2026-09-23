use clap::Parser;
use std::path::PathBuf;

/// notlin — a Kotlin-to-Java transpiler.
#[derive(Parser, Debug)]
#[command(name = "notlin", version, about)]
pub struct Cli {
    /// .kt files or directories to transpile
    pub input: Vec<PathBuf>,

    /// Workspace root whose Kotlin and Java sources provide compatibility context.
    /// Defaults to the current directory.
    #[arg(long = "root", alias = "workspace-root")]
    pub workspace_root: Option<PathBuf>,

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

    /// Assume Lombok on the target classpath: data classes emit as
    /// @Data classes with mutable fields instead of records; getter/setter
    /// hand-rolling is replaced by Lombok annotations elsewhere.
    #[arg(long)]
    pub lombok: bool,

    /// Print the tree-sitter parse tree instead of transpiling
    #[arg(long)]
    pub dump_ast: bool,

    /// Migration mode: strip translated declarations from the .kt files
    /// (deleting fully-translated ones). Implies writing Java next to input.
    #[arg(long)]
    pub in_place: bool,

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
