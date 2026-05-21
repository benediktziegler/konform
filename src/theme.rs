//! Visual theme — one code point to change the appearance of all output.
//!
//! [`ACTIVE_THEME`] is the single constant to change.  It controls:
//! * The colour scheme of `konform --help` / `konform check --help` / etc.
//!   (applied to clap via [`clap_styles`]).
//! * The colour scheme of linting output (`error[…]:`, `-->`, `help:`, …)
//!   via the [`Palette`] returned by [`palette`].
//!
//! # Adding a new theme
//! 1. Add a variant to [`Theme`].
//! 2. Add a `match` arm to [`Theme::clap_styles`] and [`palette`].
//! 3. Change [`ACTIVE_THEME`].

use clap::builder::styling::{AnsiColor, Effects, Styles};
use owo_colors::OwoColorize;

// ---------------------------------------------------------------------------
// ▸ Code point — change this one line to switch the whole visual theme
// ---------------------------------------------------------------------------

/// **Change this constant to switch the colour theme for all output.**
///
/// | Variant       | Description                                          |
/// |---------------|------------------------------------------------------|
/// | `Theme::Ruff` | Matches Ruff's colour scheme (green headers,         |
/// |               | cyan flags, bright-red errors, bright-blue arrows)   |
pub const ACTIVE_THEME: Theme = Theme::Ruff;

// ---------------------------------------------------------------------------
// Theme enum
// ---------------------------------------------------------------------------

/// Available colour themes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    /// Visual style matching Ruff 0.x.
    ///
    /// * `--help` section headers: bold green
    /// * `--help` flag names / commands: bold cyan
    /// * `--help` placeholders: cyan
    /// * Errors: bold bright-red
    /// * Warnings: bold bright-yellow
    /// * `-->` arrow and `|` pipes: bold bright-blue
    /// * `help:` label / fixable badge: bold bright-cyan
    /// * `[*]` in summary: cyan (non-bold)
    Ruff,
}

impl Theme {
    /// Build the [`Styles`] used by clap for `--help` output.
    pub fn clap_styles(self) -> Styles {
        match self {
            Theme::Ruff => Styles::styled()
                .header(AnsiColor::Green.on_default() | Effects::BOLD)
                .usage(AnsiColor::Green.on_default() | Effects::BOLD)
                .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
                .placeholder(AnsiColor::Cyan.on_default()),
        }
    }
}

// ---------------------------------------------------------------------------
// Convenience shorthands
// ---------------------------------------------------------------------------

/// Return the clap [`Styles`] for [`ACTIVE_THEME`].
///
/// Pass this to `#[command(styles = theme::clap_styles())]` in `cli.rs`.
pub fn clap_styles() -> Styles {
    ACTIVE_THEME.clap_styles()
}

/// Construct a [`Palette`] for [`ACTIVE_THEME`] with colour support
/// enabled or disabled.
pub fn palette(colors: bool) -> Palette {
    Palette {
        colors,
        theme: ACTIVE_THEME,
    }
}

// ---------------------------------------------------------------------------
// Palette — per-element colour helpers
// ---------------------------------------------------------------------------

/// A bundle of colour functions derived from a [`Theme`].
///
/// Every method returns an owned [`String`] so callers never juggle
/// borrowed Display adapters.  Colour codes are omitted when
/// `self.colors` is `false`.
pub struct Palette {
    /// Whether ANSI colour codes should be emitted.
    pub colors: bool,
    theme: Theme,
}

impl Palette {
    #[inline]
    fn p(&self, s: &str, f: impl FnOnce(&str) -> String) -> String {
        if self.colors {
            f(s)
        } else {
            s.to_owned()
        }
    }

    // ── Linting output ────────────────────────────────────────────────────

    /// `error` keyword — bold bright-red.
    pub fn error(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.bright_red().bold().to_string(),
        })
    }

    /// `warning` keyword — bold bright-yellow.
    pub fn warning(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.bright_yellow().bold().to_string(),
        })
    }

    /// `[RULE_CODE]` brackets — same colour as the level word.
    pub fn rule_brackets(&self, s: &str, is_error: bool) -> String {
        if is_error {
            self.error(s)
        } else {
            self.warning(s)
        }
    }

    /// `*` inside the `[*]` fixable badge — bold bright-cyan.
    pub fn fixable_star(&self) -> String {
        self.p("*", |s| match self.theme {
            Theme::Ruff => s.bright_cyan().bold().to_string(),
        })
    }

    /// `-->` file-location arrow — bold bright-blue.
    pub fn arrow(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.bright_blue().bold().to_string(),
        })
    }

    /// `help:` label — bold bright-cyan.
    pub fn help_label(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.bright_cyan().bold().to_string(),
        })
    }

    /// Violation message text — bold.
    pub fn message(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.bold().to_string(),
        })
    }

    /// `hint:` label — yellow.
    pub fn hint_label(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.yellow().to_string(),
        })
    }

    /// Command string in the fix hint — bold.
    pub fn hint_cmd(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.bold().to_string(),
        })
    }

    /// `*` in `[*] N fixable …` summary line — cyan, non-bold (matches ruff).
    pub fn summary_star(&self) -> String {
        self.p("*", |s| match self.theme {
            Theme::Ruff => s.cyan().to_string(),
        })
    }

    /// Flag names like `--all-files` — bold.
    #[allow(dead_code)]
    pub fn flag(&self, s: &str) -> String {
        self.p(s, |s| match self.theme {
            Theme::Ruff => s.bold().to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Colour preference  (set once from --color CLI flag)
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

/// User-specified colour output preference (from `--color`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorWhen {
    /// Emit colours when stderr is a TTY and NO_COLOR is unset (default).
    #[default]
    Auto,
    /// Always emit ANSI colours.
    Always,
    /// Never emit ANSI colours.
    Never,
}

/// Global colour preference, set once from `main()` via [`init_colors`].
pub static COLOR_PREFERENCE: OnceLock<ColorWhen> = OnceLock::new();

/// Initialise the global colour preference.
/// Must be called once, before any output is produced.
pub fn init_colors(when: ColorWhen) {
    let _ = COLOR_PREFERENCE.set(when);
}

// ---------------------------------------------------------------------------
// Log level preference  (set once from -v / -q / -s CLI flags)
// ---------------------------------------------------------------------------

/// Verbosity preference, set once from CLI flags.
///
/// Ordered from most to least verbose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// `-v` / `--verbose` — emit extra diagnostics.
    Verbose,
    /// Default — normal output.
    #[default]
    Default,
    /// `-q` / `--quiet` — print violations only; suppress hints and summary.
    Quiet,
    /// `-s` / `--silent` — suppress all output (still exits 1 on violations).
    Silent,
}

/// Global log level, set once from `main()` via [`init_log_level`].
pub static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();

/// Initialise the global log level. Must be called once before any output.
pub fn init_log_level(level: LogLevel) {
    let _ = LOG_LEVEL.set(level);
}

/// Return the current log level (defaults to [`LogLevel::Default`]).
pub fn log_level() -> LogLevel {
    LOG_LEVEL.get().copied().unwrap_or_default()
}

/// `true` when verbose mode is active.
#[allow(dead_code)]
pub fn is_verbose() -> bool {
    log_level() == LogLevel::Verbose
}

/// `true` when quiet or silent — suppress hints, summaries, and progress.
pub fn is_quiet() -> bool {
    log_level() >= LogLevel::Quiet
}

/// `true` when silent — suppress all output entirely.
pub fn is_silent() -> bool {
    log_level() >= LogLevel::Silent
}
