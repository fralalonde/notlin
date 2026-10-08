use clap::Parser;
use std::path::PathBuf;

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| "expected a positive integer".to_string())?;
    if parsed == 0 {
        return Err("expected a positive integer".to_string());
    }
    Ok(parsed)
}

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

    /// Permit explicitly diagnosed semantic approximations instead of retaining Kotlin.
    #[arg(long)]
    pub allow_approximations: bool,

    /// JSON module/toolchain configuration; compile the staged result before writing.
    #[arg(long)]
    pub validation_config: Option<PathBuf>,

    /// Nullability annotation set to emit
    #[arg(long, value_enum, default_value = "jetbrains")]
    pub annotations: Annotations,

    /// Use Lombok annotations where they preserve the planned Java behavior.
    /// Shapes requiring explicit initialization or accessors use ordinary Java.
    #[arg(long)]
    pub lombok: bool,

    /// Print the tree-sitter parse tree instead of transpiling
    #[arg(long)]
    pub dump_ast: bool,

    /// Migration mode: strip translated declarations from the .kt files
    /// (deleting fully-translated ones). Implies writing Java next to input.
    /// Directory inputs with no --out-dir use this mode automatically.
    #[arg(long)]
    pub in_place: bool,

    /// Maximum retention fixpoint passes before failing. Increase this for
    /// workspaces with unusually deep Kotlin dependency chains.
    #[arg(long, default_value_t = 64, value_parser = parse_positive_usize)]
    pub max_retention_passes: usize,

    /// Show individual file activity (-v) and trace-level details (-vv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl Cli {
    /// A directory input denotes a workspace migration. This keeps the common
    /// `notlin --lombok .` invocation useful without requiring
    /// a redundant flag, while a single file still writes to stdout unless an
    /// output mode is selected explicitly.
    pub fn migrates_in_place(&self) -> bool {
        self.in_place || (self.out_dir.is_none() && self.input.iter().any(|input| input.is_dir()))
    }
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
