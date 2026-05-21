//! Command-line interface definitions.
//!
//! `konform <PATHS>` with no subcommand is silently rewritten to
//! `konform check <PATHS>` in `main::inject_default_subcommand` so that the
//! prototype invocation style still works.

use crate::output::OutputFormat;
use crate::theme::{self, ColorWhen};
use crate::types::Level;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

fn parse_level(s: &str) -> Result<Level, String> {
    s.parse::<Level>()
}

// ---------------------------------------------------------------------------
// Top-level Cli
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "konform",
    version,
    about = "Konform: A fast and extendable multi-rule Python linter.",
    styles = theme::clap_styles(),
    after_help = "For help with a specific command, see: `konform help <command>`.",
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Control when coloured output is used.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        env = "KONFORM_COLOR",
        help_heading = "Global options"
    )]
    pub color: ColorWhen,

    /// Enable verbose logging.
    #[arg(
        short = 'v',
        long,
        global = true,
        default_value_t = false,
        help_heading = "Log levels"
    )]
    pub verbose: bool,

    /// Print violations only; suppress hints, summary, and progress output.
    #[arg(
        short = 'q',
        long,
        global = true,
        default_value_t = false,
        help_heading = "Log levels"
    )]
    pub quiet: bool,

    /// Suppress all output. Exits 1 on violations, 0 otherwise.
    #[arg(
        short = 's',
        long,
        global = true,
        default_value_t = false,
        help_heading = "Log levels"
    )]
    pub silent: bool,

    /// Ignore all configuration files; use built-in defaults.
    #[arg(
        long,
        global = true,
        default_value_t = false,
        help_heading = "Global options"
    )]
    pub isolated: bool,
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Lint Python files for rule violations (default when no subcommand given).
    Check(Box<CheckArgs>),

    /// Start the Language Server (communicates over stdin/stdout).
    Server,

    /// List or explain rules.
    Rule(RuleArgs),

    /// Print konform's version.
    Version,

    /// Clear any caches in the current directory or directories.
    Clean(CleanArgs),

    /// Initialise konform in the current directory.
    Init(InitArgs),
}

// ---------------------------------------------------------------------------
// Shared args (flattened into check)
// ---------------------------------------------------------------------------

/// Arguments shared by subcommands that operate on source files.
#[derive(Args, Debug)]
pub struct CommonArgs {
    /// Files or directories to process. Use '.' for the current directory.
    #[arg(required = true)]
    pub file_paths: Vec<PathBuf>,

    // ── Rule selection ────────────────────────────────────────────────────
    /// Enable only these rule codes or category prefixes (comma-separated).
    ///
    /// Overrides the `select` list in `[tool.konform]`.
    /// Example: `--select KIS` enables all KIS* rules.
    #[arg(long, value_delimiter = ',', help_heading = "Rule selection")]
    pub select: Vec<String>,

    /// Disable these rule codes or category prefixes (comma-separated).
    ///
    /// Merged with the `ignore` list in `[tool.konform]`.
    #[arg(long, value_delimiter = ',', help_heading = "Rule selection")]
    pub ignore: Vec<String>,

    /// Like `--select`, but adds codes on top of those already configured.
    ///
    /// Unlike `--select`, this does not override the configured list.
    /// Example: `--extend-select KPT` also runs all KPT* rules.
    #[arg(long, value_delimiter = ',', help_heading = "Rule selection")]
    pub extend_select: Vec<String>,

    /// Like `--ignore`, but adds codes on top of those already configured.
    #[arg(long, value_delimiter = ',', help_heading = "Rule selection")]
    pub extend_ignore: Vec<String>,

    // ── File selection ────────────────────────────────────────────────────
    /// Exclude files matching these glob patterns.
    ///
    /// Comma-separated. Example: `--exclude "tests/**,**/migrations/**"`
    #[arg(long, value_delimiter = ',', help_heading = "File selection")]
    pub exclude: Vec<String>,

    /// Like `--exclude`, but adds patterns on top of those already configured.
    #[arg(long, value_delimiter = ',', help_heading = "File selection")]
    pub extend_exclude: Vec<String>,

    /// Suppress specific rules for files matching a glob pattern.
    ///
    /// Format: `GLOB:CODE[,CODE,...]`.  The glob is matched against the file
    /// path relative to the project root.  Codes support the same prefix
    /// matching as `--ignore`.  Overrides the `per_file_ignores` table from
    /// the config file.
    ///
    /// Example: `--per-file-ignores "tests/**:KIS001,KPT"`
    #[arg(long, help_heading = "File selection")]
    pub per_file_ignores: Vec<String>,

    /// Like `--per-file-ignores`, but merges with the configured list instead
    /// of replacing it.
    #[arg(long, help_heading = "File selection")]
    pub extend_per_file_ignores: Vec<String>,

    // ── Global options ────────────────────────────────────────────────────
    /// Path to a custom config file (`pyproject.toml` or `konform.toml`).
    #[arg(long, help_heading = "Global options")]
    pub config: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct CheckArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    // ── Options (default heading) ─────────────────────────────────────────
    /// Filename to use when reading from stdin (`-`).
    ///
    /// Sets the displayed path for violations and is used for config-path
    /// matching (e.g. `--exclude` glob patterns).  Ignored when `-` is not
    /// in FILE_PATHS.
    #[arg(long)]
    pub stdin_filename: Option<PathBuf>,

    /// Apply auto-fixes in-place, then report any remaining violations.
    #[arg(long, default_value_t = false)]
    pub fix: bool,

    /// Print a unified diff of what `check --fix` would change to stdout.
    ///
    /// Exits 1 if any file would be modified, 0 if all files are already clean.
    #[arg(long, default_value_t = false)]
    pub diff: bool,

    /// Print the list of files konform would check, then exit 0.
    #[arg(long, default_value_t = false)]
    pub show_files: bool,

    /// Output serialisation format for violations.
    ///
    /// `json` writes a JSON array to stdout; all other formats write to stderr.
    #[arg(long, value_enum, default_value = "full")]
    pub output_format: OutputFormat,

    /// Show violation counts grouped by rule code after the summary.
    #[arg(long, default_value_t = false)]
    pub statistics: bool,

    /// Apply fixes but do not report or exit non-zero for remaining violations.
    ///
    /// Implies `--fix`.
    #[arg(long, default_value_t = false)]
    pub fix_only: bool,

    /// Exit with a non-zero status code if any files were modified by `--fix`.
    #[arg(long, default_value_t = false)]
    pub exit_non_zero_on_fix: bool,

    /// Ignore all `# noqa` suppression comments; report every violation.
    #[arg(long, default_value_t = false)]
    pub ignore_noqa: bool,

    /// Append `# noqa: CODE` to every line that has a violation, then exit 0.
    ///
    /// Lines that already carry any `# noqa` comment are left untouched.
    /// When `-` is used as a FILE_PATH, the annotated source is written to
    /// stdout instead of back to disk.
    #[arg(long, default_value_t = false)]
    pub add_noqa: bool,

    /// Write output to this file instead of stderr.
    ///
    /// For `--output-format json` the JSON is written here; for other
    /// formats the text output is written here instead of stderr.
    #[arg(short = 'o', long)]
    pub output_file: Option<PathBuf>,

    // ── Miscellaneous ─────────────────────────────────────────────────────
    /// Minimum severity level that causes a non-zero exit.
    #[arg(long, value_parser = parse_level, default_value = "error", help_heading = "Miscellaneous")]
    pub level: Level,

    /// Override the exit severity for changed-file violations. Defaults to `--level`.
    #[arg(long, value_parser = parse_level, help_heading = "Miscellaneous")]
    pub changed_files_level: Option<Level>,

    /// Bypass the file-level result cache.
    #[arg(
        short = 'n',
        long,
        default_value_t = false,
        help_heading = "Miscellaneous"
    )]
    pub no_cache: bool,

    /// Override the cache directory for this run.
    #[arg(long, help_heading = "Miscellaneous")]
    pub cache_dir: Option<PathBuf>,

    /// Exit with status code 0 even when violations are found.
    ///
    /// Useful in pre-commit hooks and editor integrations.
    #[arg(
        short = 'e',
        long,
        default_value_t = false,
        help_heading = "Miscellaneous"
    )]
    pub exit_zero: bool,

    /// Re-run the check whenever a watched `.py` file changes.
    ///
    /// Watches every path given in FILE_PATHS recursively.  Press Ctrl-C to
    /// stop.  The initial check pass runs immediately before the watch loop
    /// starts.
    #[arg(
        short = 'w',
        long,
        default_value_t = false,
        help_heading = "Miscellaneous"
    )]
    pub watch: bool,

    /// Path to write the Zuul CI `zuul_return.yaml` output.
    #[arg(
        long,
        default_value = "tmp/zuul/zuul_return.yaml",
        help_heading = "Miscellaneous"
    )]
    pub output_path: PathBuf,
}

// ---------------------------------------------------------------------------
// rule
// clean
// ---------------------------------------------------------------------------

/// Arguments for the `clean` subcommand.
#[derive(Args, Debug)]
pub struct CleanArgs {
    /// Path to a custom config file (used to discover `cache_dir`).
    #[arg(long, help_heading = "Global options")]
    pub config: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// Arguments for the `init` subcommand.
#[derive(Args, Debug)]
pub struct InitArgs {
    /// Directory to initialise. Defaults to the current working directory.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Overwrite an existing configuration (creates `konform.toml`
    /// even when `pyproject.toml` or `konform.toml` already exists).
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Skip creating `konform_patterns.toml`.
    #[arg(long = "no-patterns", default_value_t = false)]
    pub no_patterns: bool,

    /// Show what would be created or changed without writing any files.
    #[arg(long, default_value_t = false)]
    pub diff: bool,
}

// ---------------------------------------------------------------------------
// rule
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct RuleArgs {
    /// List all available rules and exit.
    #[arg(long, default_value_t = false)]
    pub list: bool,

    /// Print full documentation for a rule code and exit.
    ///
    /// Example: `konform rule --explain KIS001`
    #[arg(long)]
    pub explain: Option<String>,
}
