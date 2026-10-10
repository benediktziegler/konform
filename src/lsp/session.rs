//! Document store and per-session state.
//!
//! The `Session` is wrapped in `Arc<RwLock<Session>>` so both the main loop
//! (which mutates it on notifications) and worker threads (which read it for
//! diagnostics) can access it safely.

use crate::config::{load_config, Config};
use crate::module_probe::ModuleProbe;
use crate::rules::{all_rules, Rule};
use crate::types::Violation;
use globset::GlobSet;
use lsp_types::{TextDocumentContentChangeEvent, Uri};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The file path behind the path part of a `file://` URI. On Windows that
/// part is `/D:/a/b`, with a slash in front of the drive letter.
fn file_path(uri_path: &str) -> PathBuf {
    file_path_for(uri_path, cfg!(windows))
}

fn file_path_for(uri_path: &str, windows: bool) -> PathBuf {
    match uri_path.as_bytes() {
        [b'/', drive, b':', ..] if windows && drive.is_ascii_alphabetic() => {
            PathBuf::from(&uri_path[1..])
        }
        _ => PathBuf::from(uri_path),
    }
}

// ---------------------------------------------------------------------------
// Per-document state
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DocumentState {
    /// Current in-memory source text (kept up to date via didChange patches).
    pub source: String,
    /// LSP document version counter sent by the client.
    pub version: i32,
    /// Last diagnostics computed for this document.
    /// Stored so `codeAction` can look up fix data without a second lint pass.
    pub diagnostics: Vec<Violation>,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

pub struct Session {
    pub documents: HashMap<Uri, DocumentState>,
    pub config: Arc<Config>,
    /// Rules built from `config`, shared with request handlers. Rebuilt by
    /// [`Session::reload_config`] so that pattern files are re-read (and
    /// recompiled) only when they change, not on every request.
    pub rules: Arc<Vec<Box<dyn Rule>>>,
    pub probe: Arc<ModuleProbe>,
    /// `config`'s compiled `exclude` / `extend-exclude` globs; matching
    /// documents get no diagnostics or fixes.
    excluded: Option<GlobSet>,
    /// Working directory used for config discovery and relative path display.
    pub root: PathBuf,
}

impl Session {
    pub fn new(config: Config, probe: Arc<ModuleProbe>, root: PathBuf) -> Self {
        let rules = Arc::new(all_rules(Arc::clone(&probe), config.config_dir.clone()));
        Self {
            documents: HashMap::new(),
            excluded: config.exclude_set(),
            config: Arc::new(config),
            rules,
            probe,
            root,
        }
    }

    // ── Document lifecycle ─────────────────────────────────────────────────

    pub fn open(&mut self, uri: Uri, version: i32, text: String) {
        self.documents.insert(
            uri,
            DocumentState {
                source: text,
                version,
                diagnostics: vec![],
            },
        );
    }

    /// Apply a sequence of incremental (or full) content-change events.
    pub fn update(
        &mut self,
        uri: &Uri,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) {
        let Some(doc) = self.documents.get_mut(uri) else {
            return;
        };
        doc.version = version;
        for change in changes {
            match change.range {
                None => {
                    // Full-document replacement
                    doc.source = change.text;
                }
                Some(range) => {
                    // Incremental patch: replace the UTF-16 range with new text.
                    use crate::lsp::convert::lsp_pos_to_byte_offset;
                    let start = lsp_pos_to_byte_offset(&doc.source, range.start);
                    let end = lsp_pos_to_byte_offset(&doc.source, range.end);
                    let (start, end) = (start.min(doc.source.len()), end.min(doc.source.len()));
                    doc.source.replace_range(start..end, &change.text);
                }
            }
        }
        // Invalidate cached diagnostics after any text change.
        doc.diagnostics.clear();
    }

    pub fn close(&mut self, uri: &Uri) {
        self.documents.remove(uri);
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    pub fn source(&self, uri: &Uri) -> Option<&str> {
        self.documents.get(uri).map(|d| d.source.as_str())
    }

    /// `true` when the file behind `uri` matches the configured `exclude`.
    pub fn is_excluded(&self, uri: &Uri) -> bool {
        self.excluded
            .as_ref()
            .is_some_and(|gs| self.config.is_excluded(gs, &file_path(uri.path().as_str())))
    }

    /// Store the most recently computed diagnostics for `codeAction` lookup.
    pub fn set_diagnostics(&mut self, uri: &Uri, diags: Vec<Violation>) {
        if let Some(doc) = self.documents.get_mut(uri) {
            doc.diagnostics = diags;
        }
    }

    pub fn diagnostics(&self, uri: &Uri) -> &[Violation] {
        self.documents
            .get(uri)
            .map(|d| d.diagnostics.as_slice())
            .unwrap_or(&[])
    }

    // ── Config reload ───────────────────────────────────────────────────────────

    /// Re-read `pyproject.toml` / `konform.toml` from the session root and
    /// rebuild the rules (which re-reads any pattern files).
    /// Called when `workspace/didChangeWatchedFiles` fires.
    pub fn reload_config(&mut self) {
        self.config = Arc::new(load_config(Some(&self.root), None));
        self.excluded = self.config.exclude_set();
        self.rules = Arc::new(all_rules(
            Arc::clone(&self.probe),
            self.config.config_dir.clone(),
        ));
        eprintln!("konform server: config reloaded");
    }

    /// Configuration problems: a broken `extend` chain, or ones reported by
    /// the rules (e.g. invalid KST rules). The valid remainder keeps running.
    pub fn config_errors(&self) -> Vec<String> {
        let rule_errors = self
            .rules
            .iter()
            .flat_map(|r| r.config_errors(self.config.rule_config(r.config_name())));
        self.config
            .errors
            .iter()
            .cloned()
            .chain(rule_errors)
            .collect()
    }

    /// Merge editor-supplied workspace settings into the active [`Config`].
    ///
    /// Called when the `workspace/configuration` response arrives after a
    /// `workspace/didChangeConfiguration` notification.  Only the fields
    /// that are present in `settings` are updated; absent fields keep the
    /// values loaded from the config file.
    ///
    /// Accepted keys (mirroring `[tool.konform]` in `pyproject.toml`):
    /// - `"select"`: array of rule codes / category prefixes
    /// - `"extend-select"`: array of codes / prefixes to enable on top of the
    ///   default set (this is how opt-in rules like KNQ001 are enabled)
    /// - `"ignore"`: array of rule codes / category prefixes to suppress
    /// - `"level"`:   `"error"` or `"warning"`
    pub fn apply_editor_settings(&mut self, settings: &serde_json::Value) {
        let config = Arc::make_mut(&mut self.config);
        if let Some(arr) = settings.get("select").and_then(|v| v.as_array()) {
            config.lint.select = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect();
        }
        if let Some(arr) = settings.get("extend-select").and_then(|v| v.as_array()) {
            config.lint.extend_select = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect();
        }
        if let Some(arr) = settings.get("ignore").and_then(|v| v.as_array()) {
            config.lint.ignore = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect();
        }
        if let Some(level_str) = settings.get("level").and_then(|v| v.as_str()) {
            if let Ok(level) = level_str.parse::<crate::types::Level>() {
                config.lint.level = level;
            }
        }
        eprintln!("konform server: applied editor workspace settings");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_file_uris_lose_the_slash_before_the_drive() {
        assert_eq!(
            file_path_for("/D:/a/b.py", true),
            PathBuf::from("D:/a/b.py")
        );
        assert_eq!(file_path_for("/d:/a", true), PathBuf::from("d:/a"));
        // Unix paths, and anything that isn't a drive, are left alone.
        assert_eq!(
            file_path_for("/D:/a/b.py", false),
            PathBuf::from("/D:/a/b.py")
        );
        assert_eq!(
            file_path_for("/home/me/b.py", true),
            PathBuf::from("/home/me/b.py")
        );
        assert_eq!(file_path_for("/", true), PathBuf::from("/"));
    }
}
