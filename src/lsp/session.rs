//! Document store and per-session state.
//!
//! The `Session` is wrapped in `Arc<RwLock<Session>>` so both the main loop
//! (which mutates it on notifications) and worker threads (which read it for
//! diagnostics) can access it safely.

use crate::config::{load_config, resolve_python, Config};
use crate::module_probe::ModuleProbe;
use crate::rules::{all_rules, Rule};
use crate::types::Violation;
use globset::GlobSet;
use lsp_types::{TextDocumentContentChangeEvent, Uri};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The file path behind a `file://` URI, percent-decoded.
pub fn uri_file_path(uri: &Uri) -> PathBuf {
    file_path(uri.path().as_str())
}

/// The file path behind the path part of a `file://` URI. On Windows that
/// part is `/D:/a/b`, with a slash in front of the drive letter.
fn file_path(uri_path: &str) -> PathBuf {
    file_path_for(uri_path, cfg!(windows))
}

fn file_path_for(uri_path: &str, windows: bool) -> PathBuf {
    let decoded = percent_decode(uri_path);
    let bytes = match decoded.as_slice() {
        [b'/', drive, b':', ..] if windows && drive.is_ascii_alphabetic() => &decoded[1..],
        _ => &decoded[..],
    };
    bytes_to_path(bytes)
}

/// On Unix a file name is an arbitrary byte string, so keep the decoded bytes
/// as they are; a lossy UTF-8 conversion would name a different file.
#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Decode `%XX` escapes into raw bytes. Invalid escapes (`%zz`, a truncated
/// `%4`) are kept as-is.
fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
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
    /// What `probe` was built from, so a config reload only respawns the
    /// interpreter when it would actually change.
    probe_inputs: ProbeInputs,
    /// `config`'s compiled `exclude` / `extend-exclude` globs; matching
    /// documents get no diagnostics or fixes.
    excluded: Option<GlobSet>,
    /// Working directory used for config discovery and relative path display.
    pub root: PathBuf,
}

/// The settings a [`ModuleProbe`] is built from.
#[derive(Debug, PartialEq, Eq)]
struct ProbeInputs {
    python: PathBuf,
    root: PathBuf,
    src: Vec<String>,
}

impl ProbeInputs {
    fn new(config: &Config, session_root: &std::path::Path) -> Self {
        Self {
            python: resolve_python(config),
            root: config
                .config_dir
                .clone()
                .unwrap_or_else(|| session_root.to_path_buf()),
            src: config.src.clone(),
        }
    }
}

impl Session {
    pub fn new(config: Config, probe: Arc<ModuleProbe>, root: PathBuf) -> Self {
        let probe_inputs = ProbeInputs::new(&config, &root);
        let rules = Arc::new(all_rules(Arc::clone(&probe), config.config_dir.clone()));
        Self {
            documents: HashMap::new(),
            excluded: config.exclude_set(),
            config: Arc::new(config),
            rules,
            probe,
            probe_inputs,
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
            .is_some_and(|gs| self.config.is_excluded(gs, &uri_file_path(uri)))
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
    /// The module probe is respawned when `python`, `src` or the config
    /// directory changed.
    /// Called when `workspace/didChangeWatchedFiles` fires.
    pub fn reload_config(&mut self) {
        self.config = Arc::new(load_config(Some(&self.root), None));
        self.excluded = self.config.exclude_set();
        let inputs = ProbeInputs::new(&self.config, &self.root);
        if inputs != self.probe_inputs {
            self.probe = Arc::new(ModuleProbe::new(&inputs.python, &inputs.root, &inputs.src));
            self.probe_inputs = inputs;
        }
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

    #[test]
    fn reload_rebuilds_the_probe_only_when_its_inputs_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("konform.toml"), "ignore = [\"KIS001\"]\n").unwrap();
        let config = load_config(Some(&root), None);
        let probe = Arc::new(ModuleProbe::default());
        let mut session = Session::new(config, Arc::clone(&probe), root.clone());

        // An unrelated edit keeps the probe (and the interpreter behind it).
        session.reload_config();
        assert!(Arc::ptr_eq(&session.probe, &probe));

        // Changing `src` rebuilds it, and the rules get the new probe.
        std::fs::write(root.join("konform.toml"), "src = [\"lib\"]\n").unwrap();
        session.reload_config();
        assert!(!Arc::ptr_eq(&session.probe, &probe));
        let rebuilt = Arc::clone(&session.probe);

        // Changing `python` rebuilds it again.
        std::fs::write(
            root.join("konform.toml"),
            "src = [\"lib\"]\npython = \"other-python\"\n",
        )
        .unwrap();
        session.reload_config();
        assert!(!Arc::ptr_eq(&session.probe, &rebuilt));
        let rebuilt = Arc::clone(&session.probe);

        // Reloading the same file again does not.
        session.reload_config();
        assert!(Arc::ptr_eq(&session.probe, &rebuilt));
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(percent_decode("/my%20dir/a.py"), b"/my dir/a.py");
        // Multi-byte UTF-8 (lower and upper case hex).
        assert_eq!(
            String::from_utf8(percent_decode("/caf%C3%A9/%c3%bc.py")).unwrap(),
            "/café/ü.py"
        );
        assert_eq!(percent_decode("/plain/a.py"), b"/plain/a.py");
        assert_eq!(
            file_path_for("/D:/my%20dir/a.py", true),
            PathBuf::from("D:/my dir/a.py")
        );
        assert_eq!(file_path_for("/D%3A/a", false), PathBuf::from("/D:/a"));
    }

    #[test]
    fn an_encoded_drive_colon_is_still_a_drive() {
        assert_eq!(file_path_for("/c%3A/a", true), PathBuf::from("c:/a"));
        assert_eq!(file_path_for("/c%3A/a", false), PathBuf::from("/c:/a"));
    }

    #[test]
    fn invalid_percent_escapes_are_left_alone() {
        assert_eq!(percent_decode("/a%zz/b"), b"/a%zz/b");
        assert_eq!(percent_decode("/a%4"), b"/a%4");
        assert_eq!(percent_decode("/a%"), b"/a%");
        assert_eq!(percent_decode("/100%25"), b"/100%");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_names_survive_decoding() {
        use std::os::unix::ffi::OsStrExt;
        let path = file_path_for("/d/caf%E9.py", false);
        assert_eq!(path.as_os_str().as_bytes(), b"/d/caf\xE9.py");
        assert_eq!(percent_decode("/a%FFb"), b"/a\xFFb");
    }
}
