//! In-process LSP protocol test harness + scenario tests.
//!
//! Modeled after the in-process harnesses used by `ruff_server` and
//! `ty_server`: `main_loop` runs against one end of an in-memory
//! `lsp_server::Connection` pair (`Connection::memory()`), and tests drive
//! the other end with real `lsp_types` request/notification values. No
//! subprocess, no stdio framing — fast and deterministic.
//!
//! This complements (but does not replace) `tests/lsp_integration_test.rs`,
//! which exercises the compiled binary over real stdio.

use super::*;
use crate::config::Config;
use crate::module_probe::ModuleProbe;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread::JoinHandle;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A `konform` LSP server running `main_loop` on a background thread, driven
/// through an in-memory `Connection` pair.
struct TestServer {
    client: Connection,
    thread: Option<JoinHandle<()>>,
    next_id: i32,
    /// Canned response for the next `workspace/configuration` request the
    /// server sends us (see [`Self::set_workspace_settings`]).
    workspace_settings: Option<serde_json::Value>,
}

impl TestServer {
    fn new(root: PathBuf, config: Config) -> Self {
        let (server_conn, client_conn) = Connection::memory();
        let probe = Arc::new(ModuleProbe::default());
        let session = Arc::new(RwLock::new(Session::new(config, probe, root)));

        let thread = std::thread::spawn(move || {
            // Mirrors `lsp::run()`: the initialize handshake happens before
            // the main loop starts.
            let caps = serde_json::to_value(server_capabilities()).unwrap();
            server_conn
                .initialize(caps)
                .expect("initialize handshake failed");
            main_loop(server_conn, session);
        });

        Self {
            client: client_conn,
            thread: Some(thread),
            next_id: 1,
            workspace_settings: None,
        }
    }

    fn with_default_config(root: PathBuf) -> Self {
        Self::new(root, Config::default())
    }

    /// Queue the object the server will receive as the sole item in the
    /// result array of its next `workspace/configuration` request.
    fn set_workspace_settings(&mut self, settings: serde_json::Value) {
        self.workspace_settings = Some(settings);
    }

    /// Perform the `initialize` + `initialized` handshake and return the
    /// capabilities the server advertised.
    fn initialize(&mut self) -> ServerCapabilities {
        let result = self.request(
            "initialize",
            serde_json::json!({
                "capabilities": {},
                "processId": null,
                "rootUri": null,
            }),
        );
        self.notify("initialized", serde_json::json!({}));
        serde_json::from_value(result["capabilities"].clone())
            .expect("capabilities should deserialize")
    }

    fn fresh_id(&mut self) -> lsp_server::RequestId {
        let id = lsp_server::RequestId::from(self.next_id);
        self.next_id += 1;
        id
    }

    /// Send a request and block for its response, transparently answering
    /// any server-initiated requests that arrive first. Panics if the
    /// server responds with an error.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.try_request(method, params)
            .unwrap_or_else(|e| panic!("{method} returned an error: {e:?}"))
    }

    /// Like [`Self::request`], but returns the raw `Result` so callers can
    /// assert on error responses (e.g. `MethodNotFound`).
    fn try_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, lsp_server::ResponseError> {
        let id = self.fresh_id();
        self.client
            .sender
            .send(Message::Request(Request {
                id: id.clone(),
                method: method.to_owned(),
                params,
            }))
            .expect("send request");
        loop {
            let msg = self
                .client
                .receiver
                .recv_timeout(TIMEOUT)
                .unwrap_or_else(|_| panic!("timed out waiting for response to {method}"));
            match msg {
                Message::Response(resp) if resp.id == id => return resp.response_result,
                Message::Request(req) => self.answer_server_request(req),
                _ => {} // ignore notifications while waiting for this response
            }
        }
    }

    fn notify(&self, method: &str, params: serde_json::Value) {
        self.client
            .sender
            .send(Message::Notification(Notification::new(
                method.to_owned(),
                params,
            )))
            .expect("send notification");
    }

    /// Block until the next notification arrives, transparently answering
    /// any server-initiated requests that arrive first.
    fn next_notification(&mut self) -> Notification {
        loop {
            let msg = self
                .client
                .receiver
                .recv_timeout(TIMEOUT)
                .expect("timed out waiting for a notification");
            match msg {
                Message::Notification(n) => return n,
                Message::Request(req) => self.answer_server_request(req),
                Message::Response(_) => {}
            }
        }
    }

    /// Block until the next `textDocument/publishDiagnostics` for `uri`
    /// arrives, skipping any unrelated messages.
    fn next_diagnostics(&mut self, uri: &Uri) -> PublishDiagnosticsParams {
        loop {
            let notif = self.next_notification();
            if notif.method == "textDocument/publishDiagnostics" {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(notif.params).expect("valid publishDiagnostics params");
                if &params.uri == uri {
                    return params;
                }
            }
        }
    }

    /// Answer requests the *server* sends to the client
    /// (`client/registerCapability`, `workspace/configuration`).
    fn answer_server_request(&self, req: Request) {
        let result = match req.method.as_str() {
            "client/registerCapability" => serde_json::Value::Null,
            "workspace/configuration" => serde_json::json!([self
                .workspace_settings
                .clone()
                .unwrap_or(serde_json::Value::Null)]),
            other => panic!("unexpected server-initiated request: {other}"),
        };
        self.client
            .sender
            .send(Message::Response(Response::new_ok(req.id, result)))
            .ok();
    }

    fn open(&mut self, uri: &Uri, text: &str) {
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "python",
                    "version": 1,
                    "text": text,
                }
            }),
        );
    }

    /// Replace the whole document with `text` (full-sync `didChange`).
    fn change(&mut self, uri: &Uri, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    fn close(&mut self, uri: &Uri) {
        self.notify(
            "textDocument/didClose",
            serde_json::json!({ "textDocument": { "uri": uri } }),
        );
    }

    /// Shut the server down cleanly (`shutdown` + `exit`) and join its thread.
    fn shutdown(mut self) {
        let id = self.fresh_id();
        self.client
            .sender
            .send(Message::Request(Request {
                id: id.clone(),
                method: "shutdown".to_owned(),
                params: serde_json::Value::Null,
            }))
            .expect("send shutdown");
        loop {
            match self
                .client
                .receiver
                .recv_timeout(TIMEOUT)
                .expect("timed out waiting for shutdown response")
            {
                Message::Response(resp) if resp.id == id => break,
                Message::Request(req) => self.answer_server_request(req),
                _ => {}
            }
        }
        self.notify("exit", serde_json::Value::Null);
        if let Some(t) = self.thread.take() {
            t.join().expect("server thread panicked");
        }
    }
}

fn file_uri(path: &std::path::Path) -> Uri {
    // `path.display()` yields native separators (backslashes on Windows) and,
    // on Windows, an absolute path like `D:\a\b` rather than a leading `/`.
    // A valid `file://` URI needs forward slashes and a leading slash before
    // the path (so `D:\a\b` becomes `file:///D:/a/b`), while Unix paths
    // already start with `/` and just need `file://` prefixed as before.
    let normalized = path.display().to_string().replace('\\', "/");
    let uri_str = if normalized.starts_with('/') {
        format!("file://{normalized}")
    } else {
        format!("file:///{normalized}")
    };
    Uri::from_str(&uri_str).expect("valid file URI")
}

/// Source with a single fixable KIS001 violation.
const KIS001_SOURCE: &str = "from os.path import join\n";

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[test]
fn initialize_advertises_expected_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    let caps = server.initialize();

    assert!(caps.diagnostic_provider.is_some(), "pull diagnostics");
    assert!(caps.code_action_provider.is_some(), "code actions");
    assert!(
        matches!(caps.document_formatting_provider, Some(OneOf::Left(true))),
        "document formatting"
    );
    assert!(
        matches!(
            caps.document_range_formatting_provider,
            Some(OneOf::Left(true))
        ),
        "range formatting"
    );

    server.shutdown();
}

#[test]
fn did_open_publishes_kis001_violation() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, KIS001_SOURCE);
    let diags = server.next_diagnostics(&uri);

    assert_eq!(diags.diagnostics.len(), 1);
    assert_eq!(
        diags.diagnostics[0].code,
        Some(NumberOrString::String("KIS001".into()))
    );
    assert_eq!(
        diags.diagnostics[0].severity,
        Some(DiagnosticSeverity::ERROR)
    );

    server.shutdown();
}

#[test]
fn did_change_updates_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, "import os.path\n"); // clean
    let first = server.next_diagnostics(&uri);
    assert!(first.diagnostics.is_empty());

    server.change(&uri, 2, KIS001_SOURCE); // introduce a violation
    let second = server.next_diagnostics(&uri);
    assert_eq!(second.diagnostics.len(), 1);

    server.shutdown();
}

#[test]
fn did_close_clears_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, KIS001_SOURCE);
    let opened = server.next_diagnostics(&uri);
    assert_eq!(opened.diagnostics.len(), 1);

    server.close(&uri);
    let closed = server.next_diagnostics(&uri);
    assert!(
        closed.diagnostics.is_empty(),
        "didClose should clear diagnostics"
    );

    server.shutdown();
}

#[test]
fn pull_diagnostic_matches_push_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, KIS001_SOURCE);
    let pushed = server.next_diagnostics(&uri);

    let result = server.request(
        "textDocument/diagnostic",
        serde_json::json!({ "textDocument": { "uri": uri } }),
    );
    let report: DocumentDiagnosticReportResult = serde_json::from_value(result).unwrap();
    let DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) = report
    else {
        panic!("expected a full diagnostic report");
    };

    assert_eq!(
        full.full_document_diagnostic_report.items.len(),
        pushed.diagnostics.len()
    );
    assert_eq!(
        full.full_document_diagnostic_report.items[0].code,
        pushed.diagnostics[0].code
    );

    server.shutdown();
}

#[test]
fn code_action_offers_quickfix_and_fix_all() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, KIS001_SOURCE);
    server.next_diagnostics(&uri); // drain the push

    let result = server.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 0 },
            },
            "context": { "diagnostics": [] },
        }),
    );
    let actions: Vec<CodeActionOrCommand> = serde_json::from_value(result).unwrap();
    assert!(!actions.is_empty(), "expected at least one code action");

    let kinds: Vec<Option<CodeActionKind>> = actions
        .iter()
        .map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.kind.clone(),
            CodeActionOrCommand::Command(_) => None,
        })
        .collect();
    assert!(
        kinds
            .iter()
            .any(|k| k.as_ref() == Some(&CodeActionKind::new("source.fixAll.konform"))),
        "expected a source.fixAll.konform action: {kinds:?}"
    );
    assert!(
        kinds
            .iter()
            .any(|k| k.as_ref() == Some(&CodeActionKind::QUICKFIX)),
        "expected a quickfix action: {kinds:?}"
    );

    server.shutdown();
}

#[test]
fn formatting_applies_fixer_to_whole_document() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, KIS001_SOURCE);
    server.next_diagnostics(&uri); // drain the push

    let result = server.request(
        "textDocument/formatting",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 4, "insertSpaces": true },
        }),
    );
    let edits: Vec<TextEdit> = serde_json::from_value(result).unwrap();
    assert_eq!(edits.len(), 1, "expected a single full-document edit");
    assert!(
        !edits[0].new_text.contains("from os.path import join"),
        "formatting should remove the violation"
    );

    server.shutdown();
}

#[test]
fn workspace_configuration_round_trip_updates_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, KIS001_SOURCE);
    let before = server.next_diagnostics(&uri);
    assert_eq!(before.diagnostics.len(), 1);

    // Tell the server (when it asks) that the editor now wants KIS001 ignored.
    server.set_workspace_settings(serde_json::json!({ "ignore": ["KIS001"] }));
    server.notify("workspace/didChangeConfiguration", serde_json::json!({}));

    let after = server.next_diagnostics(&uri);
    assert!(
        after.diagnostics.is_empty(),
        "KIS001 should be suppressed after applying editor settings"
    );

    server.shutdown();
}

#[test]
fn did_change_watched_files_reloads_config_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let uri = file_uri(&dir.path().join("mod.py"));
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    server.open(&uri, KIS001_SOURCE);
    let before = server.next_diagnostics(&uri);
    assert_eq!(before.diagnostics.len(), 1);

    std::fs::write(dir.path().join("konform.toml"), "ignore = [\"KIS001\"]\n").unwrap();
    server.notify(
        "workspace/didChangeWatchedFiles",
        serde_json::json!({ "changes": [] }),
    );

    let after = server.next_diagnostics(&uri);
    assert!(
        after.diagnostics.is_empty(),
        "config reload should pick up the new ignore list"
    );

    server.shutdown();
}

#[test]
fn unknown_request_returns_method_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    let err = server
        .try_request("textDocument/hover", serde_json::json!({}))
        .expect_err("hover is not implemented");
    assert_eq!(err.code, lsp_server::ErrorCode::MethodNotFound as i32);

    server.shutdown();
}

#[test]
fn malformed_params_do_not_crash_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = TestServer::with_default_config(dir.path().to_path_buf());
    server.initialize();

    // Missing required `textDocument` field.
    let err = server
        .try_request("textDocument/diagnostic", serde_json::json!({}))
        .expect_err("malformed params should error, not panic");
    assert_eq!(err.code, lsp_server::ErrorCode::InternalError as i32);

    // The server must still be alive and answer a well-formed request.
    let uri = file_uri(&dir.path().join("mod.py"));
    server.open(&uri, KIS001_SOURCE);
    let diags = server.next_diagnostics(&uri);
    assert_eq!(diags.diagnostics.len(), 1);

    server.shutdown();
}
