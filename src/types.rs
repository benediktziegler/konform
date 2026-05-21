// Shared types referenced by all modules.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Level
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warning,
    Error,
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Level::Warning => write!(f, "warning"),
            Level::Error => write!(f, "error"),
        }
    }
}

impl std::str::FromStr for Level {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "warning" => Ok(Level::Warning),
            "error" => Ok(Level::Error),
            other => Err(format!("Unknown level: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Violation  (Step 3 — typed replacement for the serde_json::Value blobs)
// ---------------------------------------------------------------------------

/// A single rule violation produced by a checker.
///
/// This is the typed equivalent of the `serde_json::Value` dicts that
/// `checker.rs` currently returns.  The old dict format is preserved in
/// `to_json()` so the cache, output, and LSP layers can migrate
/// incrementally without a flag day.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    /// Rule code, e.g. `"FMIS001"` or `"FMPT001"`.
    pub rule: String,
    /// 1-based line number of the violation.
    pub line: usize,
    /// 0-based byte column of the start of the offending token.
    pub col: usize,
    /// 1-based line number of the last line of the violating construct.
    pub end_line: usize,
    /// 0-based byte column of the end of the offending token (0 = unknown).
    pub end_col: usize,
    /// Short human-readable description shown to the user.
    pub message: String,
    /// Optional longer explanation / fix hint.
    pub help: Option<String>,
    /// Severity of this violation.
    pub level: Level,
    /// Whether the violation can be automatically fixed.
    pub fixable: bool,
}

#[allow(dead_code)]
impl Violation {
    /// Serialise to the Zuul / cache dict format used by the current code.
    ///
    /// Keeping this format stable lets the cache, output, and LSP layers
    /// continue working without changes while the engine refactor
    /// (Steps 4–11) progresses.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "rule":    self.rule,
            "line":    self.line,
            "message": self.message,
            "level":   self.level.to_string(),
            "help":    self.help,
            "range": {
                "start_line":      self.line,
                // 1-based in the wire format (col is 0-based internally)
                "start_character": self.col + 1,
                "end_line":        self.end_line,
                "end_character":   self.end_col,
            },
            "fixable": self.fixable,
        })
    }

    /// Round-trip from the legacy `serde_json::Value` dict (best-effort).
    ///
    /// Used by the migration shim so call sites that already produce
    /// `serde_json::Value` blobs can construct a `Violation` without being
    /// fully ported yet.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Violation {
            rule: v["message"]
                .as_str()
                .and_then(|m| m.split(':').next())
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|| "UNKNOWN".to_owned()),
            line: v["line"].as_u64()? as usize,
            col: v["range"]["start_character"]
                .as_u64()
                .unwrap_or(1)
                .saturating_sub(1) as usize,
            end_line: v["range"]["end_line"].as_u64().unwrap_or(1) as usize,
            end_col: v["range"]["end_character"].as_u64().unwrap_or(0) as usize,
            message: v["message"].as_str().unwrap_or("").to_owned(),
            help: v.get("help").and_then(|h| h.as_str()).map(str::to_owned),
            level: v["level"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(Level::Error),
            fixable: v["fixable"].as_bool().unwrap_or(false),
        })
    }
}

// ---------------------------------------------------------------------------
// ChangedFiles
// ---------------------------------------------------------------------------

/// The set of files considered "changed" for violation routing.
#[derive(Debug, Clone)]
pub struct ChangedFiles {
    pub files: HashSet<String>,
}

impl ChangedFiles {
    pub fn contains(&self, path: &str) -> bool {
        self.files.contains(path)
    }
}
