//! Document store and per-session state.
//!
//! The `Session` is wrapped in `Arc<RwLock<Session>>` so both the main loop
//! (which mutates it on notifications) and worker threads (which read it for
//! diagnostics) can access it safely.

use crate::config::{load_config, Config};
use crate::module_probe::ModuleProbe;
use crate::types::Violation;
use lsp_types::{TextDocumentContentChangeEvent, Uri};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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
    pub config: Config,
    pub probe: Arc<ModuleProbe>,
    /// Working directory used for config discovery and relative path display.
    pub root: PathBuf,
}

impl Session {
    pub fn new(config: Config, probe: Arc<ModuleProbe>, root: PathBuf) -> Self {
        Self {
            documents: HashMap::new(),
            config,
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

    /// Re-read `pyproject.toml` / `konform.toml` from the session root.
    /// Called when `workspace/didChangeWatchedFiles` fires.
    pub fn reload_config(&mut self) {
        self.config = load_config(Some(&self.root), None);
        eprintln!("konform server: config reloaded");
    }

    /// Merge editor-supplied workspace settings into the active [`Config`].
    ///
    /// Called when the `workspace/configuration` response arrives after a
    /// `workspace/didChangeConfiguration` notification.  Only the fields
    /// that are present in `settings` are updated; absent fields keep the
    /// values loaded from the config file.
    ///
    /// Accepted keys (mirroring `[tool.konform]` in `pyproject.toml`):
    /// - `"select"` — array of rule codes / category prefixes
    /// - `"ignore"` — array of rule codes / category prefixes to suppress
    /// - `"level"`  — `"error"` or `"warning"`
    pub fn apply_editor_settings(&mut self, settings: &serde_json::Value) {
        if let Some(arr) = settings.get("select").and_then(|v| v.as_array()) {
            self.config.select = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect();
        }
        if let Some(arr) = settings.get("ignore").and_then(|v| v.as_array()) {
            self.config.ignore = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect();
        }
        if let Some(level_str) = settings.get("level").and_then(|v| v.as_str()) {
            if let Ok(level) = level_str.parse::<crate::types::Level>() {
                self.config.level = level;
            }
        }
        eprintln!("konform server: applied editor workspace settings");
    }
}
