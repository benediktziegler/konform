//! LSP message dispatcher and all request / notification handlers.
//!
//! The main loop is **single-threaded and synchronous**.  Lint runs happen
//! inline in the loop; they are fast (~1–5 ms per file) so no worker pool is
//! needed at this stage.  A rayon worker pool can be wired in later once the
//! engine.rs refactor (plan §2) is complete.
//!
//! Diagnostics are **pull-based** (LSP 3.17 `textDocument/diagnostic`).
//! The client requests them when it needs them; the server runs the linter
//! on the in-memory source and returns results immediately.
//!
//! Additionally, diagnostics are **pushed** via `textDocument/publishDiagnostics`
//! after every `didOpen` and `didChange` so editors that don't support pull
//! diagnostics still receive results.

use super::convert::{full_document_edit, violation_to_diagnostic};
use super::session::Session;
use crate::engine::{run_check, run_fix, CheckInput};
use crate::rules::{all_rules, Rule};
use crate::types::Violation;
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// ---------------------------------------------------------------------------
// Pending server-request tracking
// ---------------------------------------------------------------------------

/// Identifies the purpose of a server-initiated request awaiting a response.
enum PendingKind {
    /// A `workspace/configuration` request sent after `workspace/didChangeConfiguration`.
    WorkspaceConfiguration,
}

/// Maps request IDs that the server sent to the client to their [`PendingKind`].
type PendingMap = HashMap<lsp_server::RequestId, PendingKind>;

// ---------------------------------------------------------------------------
// Server capabilities advertised at initialize
// ---------------------------------------------------------------------------

/// Build the [`ServerCapabilities`] advertised during the `initialize` handshake.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // Incremental sync: client sends only the changed ranges on didChange.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),

        // Pull diagnostics (LSP 3.17).  Editors that don't support pull
        // will still get push diagnostics via publishDiagnostics below.
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("konform".into()),
            inter_file_dependencies: false,
            workspace_diagnostics: false,
            work_done_progress_options: Default::default(),
        })),

        // Code actions: quick-fix per violation + source.fixAll.
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![
                CodeActionKind::QUICKFIX,
                CodeActionKind::new("source.fixAll.konform"),
            ]),
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        })),

        // Whole-document and range formatting (runs the IS001 fixer).
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),

        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

/// Process LSP messages until the client sends `shutdown` + `exit`.
pub fn main_loop(connection: Connection, session: Arc<RwLock<Session>>) {
    let mut pending: PendingMap = HashMap::new();
    // Counter for IDs on outgoing server-initiated requests.  Starts at 1
    // and increments monotonically; distinct from client-request IDs which
    // are managed by the client.
    let mut next_id: u32 = 1;

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                // handle_shutdown sends the shutdown response and returns true
                // when the client has requested shutdown.
                if connection.handle_shutdown(&req).unwrap_or(false) {
                    return;
                }
                handle_request(&connection, &session, req);
            }
            Message::Notification(notif) => {
                handle_notification(&connection, &session, notif, &mut pending, &mut next_id);
            }
            Message::Response(resp) => {
                // Dispatch to the registered handler if we sent this request.
                if let Some(kind) = pending.remove(&resp.id) {
                    handle_pending_response(
                        kind,
                        resp.response_result
                            .clone()
                            .unwrap_or(serde_json::Value::Null),
                        &connection,
                        &session,
                    );
                }
                // Responses to other server-initiated requests (e.g.
                // client/registerCapability) are silently discarded.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

fn handle_request(connection: &Connection, session: &Arc<RwLock<Session>>, req: Request) {
    let id = req.id.clone();
    let method = req.method.as_str();

    let result: anyhow::Result<serde_json::Value> = match method {
        "textDocument/diagnostic" => handle_diagnostic(session, req.params),
        "textDocument/codeAction" => handle_code_action(session, req.params),
        "textDocument/formatting" => handle_formatting(session, req.params),
        "textDocument/rangeFormatting" => handle_range_formatting(session, req.params),
        _ => {
            // Unknown request — send MethodNotFound.
            let resp = Response::new_err(
                id,
                lsp_server::ErrorCode::MethodNotFound as i32,
                format!("method not found: {method}"),
            );
            connection.sender.send(Message::Response(resp)).ok();
            return;
        }
    };

    let resp = match result {
        Ok(v) => Response::new_ok(id, v),
        Err(e) => Response::new_err(
            id,
            lsp_server::ErrorCode::InternalError as i32,
            e.to_string(),
        ),
    };
    connection.sender.send(Message::Response(resp)).ok();
}

// ---------------------------------------------------------------------------
// Request handlers
// ---------------------------------------------------------------------------

/// `textDocument/diagnostic` — pull-model diagnostics (LSP 3.17).
fn handle_diagnostic(
    session: &Arc<RwLock<Session>>,
    params: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let params: DocumentDiagnosticParams = serde_json::from_value(params)?;
    let uri = &params.text_document.uri;

    let (violations, lsp_diags) = run_diagnostics(session, uri);

    // Cache violations for codeAction.
    session.write().unwrap().set_diagnostics(uri, violations);

    let report = DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
        related_documents: None,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: None,
            items: lsp_diags,
        },
    });
    Ok(serde_json::to_value(report)?)
}

/// `textDocument/codeAction` — return fix actions for violations in range.
///
/// Offers two tiers of code action:
/// 1. **"Fix All"** (`source.fixAll.konform`) — runs the fixer on the whole
///    document and replaces it with a single full-document [`TextEdit`].
/// 2. **Per-violation quick-fix** (`quickfix`) — for each fixable [`Violation`]
///    whose source line intersects the requested range, computes a minimal
///    [`TextEdit`] via [`violation_fix_edit`] and attaches it directly.
fn handle_code_action(
    session: &Arc<RwLock<Session>>,
    params: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let params: CodeActionParams = serde_json::from_value(params)?;
    let uri = &params.text_document.uri;

    // Grab everything we need from the session in a single critical section
    // so we don't hold the lock during expensive fix computations.
    let (source, config, probe, cached_violations) = {
        let sess = session.read().unwrap();
        let source = match sess.source(uri) {
            Some(s) => s.to_owned(),
            None => return Ok(serde_json::json!([])),
        };
        let violations = sess.diagnostics(uri).to_vec();
        (
            source,
            sess.config.clone(),
            Arc::clone(&sess.probe),
            violations,
        )
    };

    let path: std::path::PathBuf = uri.path().as_str().into();
    let rules = all_rules(Arc::clone(&probe), config.config_dir.clone());
    let mut actions: Vec<CodeActionOrCommand> = Vec::new();

    // ── Tier 1: "Fix All" ─────────────────────────────────────────────────
    let fix_input = CheckInput::new(&path, &source);
    if let Ok(Some(fixed)) = run_fix(&fix_input, &rules, &config) {
        let edit = full_document_edit(&source, &fixed);
        // lsp_types::Uri uses fluent-uri internally; clippy flags it as a
        // "mutable key type" but it is structurally immutable once created.
        #[allow(clippy::mutable_key_type)]
        let mut changes = HashMap::new();
        changes.insert(uri.clone(), vec![edit]);
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: "Fix all konform violations".to_owned(),
            kind: Some(CodeActionKind::new("source.fixAll.konform")),
            edit: Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }),
            is_preferred: Some(true),
            ..Default::default()
        }));
    }

    // ── Tier 2: per-violation quick-fix ───────────────────────────────────
    let req_range = params.range;
    for violation in &cached_violations {
        if !violation.fixable {
            continue;
        }
        let viol_line = violation.line.saturating_sub(1) as u32;
        if viol_line < req_range.start.line || viol_line > req_range.end.line {
            continue;
        }
        let diag = violation_to_diagnostic(violation);
        let title = format!("[{}] {}", violation.rule, violation.message);
        if let Some(fix_edits) = violation_fix_edit(&source, violation, &rules, &config) {
            #[allow(clippy::mutable_key_type)]
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), fix_edits);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diag]),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }),
                is_preferred: Some(false),
                ..Default::default()
            }));
        }
    }

    Ok(serde_json::to_value(actions)?)
}

/// Build the complete set of [`TextEdit`]s that fix the given `violation`.
///
/// A KIS001 fix touches **multiple** locations in one pass: it removes (or
/// replaces) the import declaration, inserts a new import statement, and
/// renames every usage of the old local name throughout the file.  Returning
/// only the single hunk at the violation’s line would leave the file broken
/// (missing new import + un-renamed usages), so this function collects **all**
/// hunks produced by running the fixer with only this rule enabled.
///
/// Returns `None` when the fixer produces no change at all.
fn violation_fix_edit(
    source: &str,
    violation: &Violation,
    rules: &[Box<dyn Rule>],
    config: &crate::config::Config,
) -> Option<Vec<TextEdit>> {
    use similar::{ChangeTag, TextDiff};

    // Narrow the config to run only this rule’s fixer.
    let mut cfg = config.clone();
    cfg.select = vec![violation.rule.clone()];
    cfg.ignore.clear();

    let path = std::path::PathBuf::from("<lsp-fix>");
    let input = CheckInput::new(&path, source);
    let fixed = run_fix(&input, rules, &cfg).ok()??;

    // Diff line-by-line and collect ALL (old_start, old_end_exclusive, new_text)
    // hunks — not just the one at the violation’s line.  A KIS001 fix spans at
    // least three distinct locations (import removal, new import insertion, usage
    // renames), so every hunk must be included to keep the file consistent.
    let diff = TextDiff::from_lines(source, &fixed);

    let mut edits: Vec<TextEdit> = Vec::new();
    let mut old_line = 0usize;
    let mut hunk_start: Option<usize> = None;
    let mut hunk_old_end = 0usize;
    let mut hunk_new = String::new();

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => {
                hunk_start.get_or_insert(old_line);
                hunk_old_end = old_line + 1;
                old_line += 1;
            }
            ChangeTag::Insert => {
                hunk_start.get_or_insert(old_line);
                hunk_new.push_str(change.value());
            }
            ChangeTag::Equal => {
                if let Some(start) = hunk_start.take() {
                    let end = hunk_old_end.max(start + 1);
                    edits.push(TextEdit {
                        range: Range {
                            start: Position {
                                line: start as u32,
                                character: 0,
                            },
                            end: Position {
                                line: end as u32,
                                character: 0,
                            },
                        },
                        new_text: std::mem::take(&mut hunk_new),
                    });
                    hunk_old_end = 0;
                }
                old_line += 1;
            }
        }
    }
    // Flush a trailing hunk (file doesn’t end with Equal).
    if let Some(start) = hunk_start {
        let end = hunk_old_end.max(start + 1);
        edits.push(TextEdit {
            range: Range {
                start: Position {
                    line: start as u32,
                    character: 0,
                },
                end: Position {
                    line: end as u32,
                    character: 0,
                },
            },
            new_text: hunk_new,
        });
    }

    if edits.is_empty() {
        None
    } else {
        Some(edits)
    }
}

/// `textDocument/formatting` — apply all fixable violations to the whole document.
fn handle_formatting(
    session: &Arc<RwLock<Session>>,
    params: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let params: DocumentFormattingParams = serde_json::from_value(params)?;
    let uri = &params.text_document.uri;

    let sess = session.read().unwrap();
    let source = match sess.source(uri) {
        Some(s) => s.to_owned(),
        None => return Ok(serde_json::Value::Null),
    };
    let config = sess.config.clone();
    let probe = Arc::clone(&sess.probe);
    drop(sess);

    let path: std::path::PathBuf = uri.path().as_str().into();
    let rules = all_rules(probe, config.config_dir.clone());
    let fix_input = CheckInput::new(&path, &source);
    let edits: Vec<TextEdit> = match run_fix(&fix_input, &rules, &config) {
        Ok(Some(fixed)) => vec![full_document_edit(&source, &fixed)],
        _ => vec![],
    };
    Ok(serde_json::to_value(edits)?)
}

/// `textDocument/rangeFormatting` — currently delegates to full-document formatting.
///
/// A range-aware implementation would run the fixer and filter edits to the
/// requested range. For now, full formatting is correct (the fixer is
/// idempotent and only touches import lines).
fn handle_range_formatting(
    session: &Arc<RwLock<Session>>,
    params: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let params: DocumentRangeFormattingParams = serde_json::from_value(params)?;
    // Delegate to full-document formatting.
    handle_formatting(
        session,
        serde_json::to_value(DocumentFormattingParams {
            text_document: params.text_document,
            options: params.options,
            work_done_progress_params: Default::default(),
        })?,
    )
}

// ---------------------------------------------------------------------------
// Notification dispatch
// ---------------------------------------------------------------------------

fn handle_notification(
    connection: &Connection,
    session: &Arc<RwLock<Session>>,
    notif: Notification,
    pending: &mut PendingMap,
    next_id: &mut u32,
) {
    match notif.method.as_str() {
        "initialized" => {
            // Register file watchers for config files so we can reload on change.
            register_watchers(connection);
        }
        "textDocument/didOpen" => {
            if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(notif.params) {
                let uri = p.text_document.uri.clone();
                let version = p.text_document.version;
                let text = p.text_document.text.clone();
                session.write().unwrap().open(uri.clone(), version, text);
                push_diagnostics(connection, session, &uri);
            }
        }
        "textDocument/didChange" => {
            if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(notif.params) {
                let uri = p.text_document.uri.clone();
                let version = p.text_document.version;
                session
                    .write()
                    .unwrap()
                    .update(&uri, version, p.content_changes);
                push_diagnostics(connection, session, &uri);
            }
        }
        "textDocument/didClose" => {
            if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(notif.params) {
                session.write().unwrap().close(&p.text_document.uri);
                // Clear diagnostics in the editor when the file is closed.
                publish_empty_diagnostics(connection, &p.text_document.uri);
            }
        }
        "textDocument/didSave" => {
            // Re-lint on save (picks up any changes made outside the editor).
            if let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(notif.params) {
                push_diagnostics(connection, session, &p.text_document.uri);
            }
        }
        "workspace/didChangeWatchedFiles" => {
            // A config file changed — reload and re-lint all open documents.
            let uris: Vec<Uri> = {
                let mut sess = session.write().unwrap();
                sess.reload_config();
                // Collect open document URIs (can't borrow sess while pushing diags).
                sess.documents.keys().cloned().collect()
            };
            for uri in uris {
                push_diagnostics(connection, session, &uri);
            }
        }
        "workspace/didChangeConfiguration" => {
            // The editor signals that its settings have changed.  Send a
            // workspace/configuration request to fetch the current "konform"
            // section, then apply the returned settings and re-lint.
            let id = lsp_server::RequestId::from(*next_id as i32);
            *next_id += 1;
            let params = serde_json::json!({
                "items": [{"section": "konform"}]
            });
            let req = Request {
                id: id.clone(),
                method: "workspace/configuration".to_owned(),
                params,
            };
            connection.sender.send(Message::Request(req)).ok();
            pending.insert(id, PendingKind::WorkspaceConfiguration);
        }
        _ => {} // unknown notification — ignore
    }
}

// ---------------------------------------------------------------------------
// Pending-response handler
// ---------------------------------------------------------------------------

/// Process the client's response to a server-initiated [`workspace/configuration`] request.
///
/// Called from `main_loop` when a [`Message::Response`] arrives whose ID
/// is registered in the pending map as [`PendingKind::WorkspaceConfiguration`].
fn handle_pending_response(
    kind: PendingKind,
    result: serde_json::Value,
    connection: &Connection,
    session: &Arc<RwLock<Session>>,
) {
    match kind {
        PendingKind::WorkspaceConfiguration => {
            // The result is an array with one element per requested item.
            // We requested one item ({"section": "konform"}), so result[0]
            // holds the konform settings object (or null/absent if unsupported).
            let settings = result
                .as_array()
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            if !settings.is_object() {
                // Client returned null or an empty array — nothing to apply.
                return;
            }

            session.write().unwrap().apply_editor_settings(&settings);

            // Re-lint all open documents with the updated settings.
            let uris: Vec<Uri> = session.read().unwrap().documents.keys().cloned().collect();
            for uri in uris {
                push_diagnostics(connection, session, &uri);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Diagnostics helpers
// ---------------------------------------------------------------------------

/// Run the linter on the in-memory source of `uri` and return
/// `(violations, lsp_diagnostics)`.
fn run_diagnostics(
    session: &Arc<RwLock<Session>>,
    uri: &Uri,
) -> (Vec<Violation>, Vec<lsp_types::Diagnostic>) {
    let sess = session.read().unwrap();
    let source = match sess.source(uri) {
        Some(s) => s.to_owned(),
        None => return (vec![], vec![]),
    };
    let config = sess.config.clone();
    let probe = Arc::clone(&sess.probe);
    drop(sess);

    let path: std::path::PathBuf = uri.path().as_str().into();
    let rules = all_rules(Arc::clone(&probe), config.config_dir.clone());
    let input = CheckInput::new(&path, &source);
    let violations: Vec<Violation> = run_check(&input, &rules, &config);
    let lsp_diags = violations.iter().map(violation_to_diagnostic).collect();
    (violations, lsp_diags)
}

/// Compute diagnostics and push them via `textDocument/publishDiagnostics`.
fn push_diagnostics(connection: &Connection, session: &Arc<RwLock<Session>>, uri: &Uri) {
    let (violations, lsp_diags) = run_diagnostics(session, uri);
    session.write().unwrap().set_diagnostics(uri, violations);

    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: lsp_diags,
        version: None,
    };
    let notif = Notification::new("textDocument/publishDiagnostics".to_owned(), params);
    connection.sender.send(Message::Notification(notif)).ok();
}

/// Send an empty `textDocument/publishDiagnostics` to clear editor decorations.
fn publish_empty_diagnostics(connection: &Connection, uri: &Uri) {
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: vec![],
        version: None,
    };
    let notif = Notification::new("textDocument/publishDiagnostics".to_owned(), params);
    connection.sender.send(Message::Notification(notif)).ok();
}

/// Dynamically register file watchers for `pyproject.toml` and `konform.toml`
/// and opt-in to `workspace/didChangeConfiguration` notifications after the
/// server is initialized.
fn register_watchers(connection: &Connection) {
    use lsp_types::{FileSystemWatcher, GlobPattern, Registration, RegistrationParams, WatchKind};

    let watchers = vec![
        FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/pyproject.toml".into()),
            kind: Some(WatchKind::all()),
        },
        FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/konform.toml".into()),
            kind: Some(WatchKind::all()),
        },
    ];

    // Register file-change watchers (for pyproject.toml / konform.toml).
    let file_watcher_reg = Registration {
        id: "konform-watch-config".into(),
        method: "workspace/didChangeWatchedFiles".into(),
        register_options: Some(
            serde_json::to_value(lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers })
                .unwrap_or_default(),
        ),
    };

    // Also register for workspace/didChangeConfiguration so that editors
    // notify us when the "konform" settings section changes.
    let settings_change_reg = Registration {
        id: "konform-watch-settings".into(),
        method: "workspace/didChangeConfiguration".into(),
        register_options: None,
    };

    let params = RegistrationParams {
        registrations: vec![file_watcher_reg, settings_change_reg],
    };
    let req_id = lsp_server::RequestId::from("konform-watch-config".to_owned());
    let req = Request {
        id: req_id,
        method: "client/registerCapability".into(),
        params: serde_json::to_value(params).unwrap_or_default(),
    };
    connection.sender.send(Message::Request(req)).ok();
}
