//! End-to-end LSP smoke test over **real stdio**, against the compiled
//! binary, mirroring the blackbox approach used by tools like `pytest-lsp`.
//!
//! This intentionally duplicates only a thin slice of what
//! `src/lsp/handler/tests.rs`'s in-process harness already covers in depth.
//! Its job is narrower: catch bugs the in-process harness *can't* see:
//! stdio framing, process startup/argv handling, and clean shutdown/exit,
//! which is exactly the class of bug that can make the server "not work"
//! when a real editor (e.g. Zed) launches it as a subprocess.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

/// Write a single framed JSON-RPC message (`Content-Length` header + body).
fn send(stdin: &mut ChildStdin, method: &str, id: Option<i64>, params: Value) {
    let mut msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    if let Some(id) = id {
        msg["id"] = json!(id);
    }
    let body = serde_json::to_string(&msg).expect("serializable message");
    write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).expect("write to stdin");
    stdin.flush().expect("flush stdin");
}

/// Read a single framed JSON-RPC message from the server's stdout.
fn read_message(reader: &mut impl BufRead) -> Value {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read header line");
        assert!(n > 0, "stdout closed before a full header was received");
        if line == "\r\n" {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = Some(v.trim().parse().expect("valid Content-Length"));
        }
    }
    let len = content_length.expect("message must have a Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read message body");
    serde_json::from_slice(&buf).expect("valid JSON-RPC message")
}

/// Messages from the server, read on a background thread so that a test
/// waiting for a message that never comes fails instead of hanging CI.
struct Messages(std::sync::mpsc::Receiver<Value>);

impl Messages {
    fn spawn(mut reader: BufReader<std::process::ChildStdout>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(msg) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_message(&mut reader)))
            {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Self(rx)
    }

    fn next(&self) -> Value {
        self.0
            .recv_timeout(Duration::from_secs(30))
            .expect("no message from the server within 30 s (or it exited)")
    }
}

/// Read messages until one whose `method` matches `method`, discarding
/// anything else (e.g. server-initiated `client/registerCapability`).
fn read_notification(reader: &Messages, method: &str) -> Value {
    for _ in 0..20 {
        let msg = reader.next();
        if msg.get("method").and_then(Value::as_str) == Some(method) {
            return msg;
        }
    }
    panic!("did not observe a {method} notification within 20 messages");
}

/// Build a `file://` URI from a path, valid on both Unix and Windows.
///
/// `path.display()` yields native separators (backslashes on Windows) and,
/// on Windows, an absolute path like `D:\a\b` rather than a leading `/`.
/// Normalize to forward slashes and ensure exactly one leading slash so the
/// result is a well-formed `file://` URI on either platform.
fn file_uri(path: &std::path::Path) -> String {
    let normalized = path.display().to_string().replace('\\', "/");
    if normalized.starts_with('/') {
        format!("file://{normalized}")
    } else {
        format!("file:///{normalized}")
    }
}

struct ServerProcess {
    root: std::path::PathBuf,
    child: Child,
    stdin: ChildStdin,
    reader: Messages,
}

impl ServerProcess {
    fn spawn(root: &std::path::Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_konform");
        let mut child = Command::new(bin)
            .arg("server")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn `konform server`");
        let stdin = child.stdin.take().expect("piped stdin");
        let reader = Messages::spawn(BufReader::new(child.stdout.take().expect("piped stdout")));
        Self {
            root: root.to_path_buf(),
            child,
            stdin,
            reader,
        }
    }

    /// Handshake as a client that can watch files for the server.
    fn initialize(&mut self) -> Value {
        let caps =
            json!({ "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } } });
        let (resp, reg) = self.initialize_with(caps);
        // The server registers its config/rule-file watchers right after the
        // handshake (regression guard: this used to never happen).
        let watchers = reg["params"]["registrations"][0]["registerOptions"]["watchers"].to_string();
        assert!(watchers.contains("konform_rules.toml"), "{watchers}");
        resp
    }

    /// Handshake with `capabilities`; returns the initialize response and
    /// the server's `client/registerCapability` request (answered).
    fn initialize_with(&mut self, capabilities: Value) -> (Value, Value) {
        send(
            &mut self.stdin,
            "initialize",
            Some(1),
            json!({ "processId": null, "rootUri": file_uri(&self.root), "capabilities": capabilities }),
        );
        let resp = self.reader.next();
        assert_eq!(resp["id"], 1, "unexpected response id: {resp}");
        send(&mut self.stdin, "initialized", None, json!({}));
        let reg = self.reader.next();
        assert_eq!(reg["method"], "client/registerCapability", "{reg}");
        let body = json!({ "jsonrpc": "2.0", "id": reg["id"], "result": null }).to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
        (resp, reg)
    }

    fn shutdown_and_exit(mut self) {
        send(&mut self.stdin, "shutdown", Some(999), Value::Null);
        let resp = self.reader.next();
        assert_eq!(resp["id"], 999, "unexpected response to shutdown: {resp}");
        send(&mut self.stdin, "exit", None, Value::Null);

        let status = self
            .child
            .wait()
            .expect("server process should exit cleanly");
        assert!(status.success(), "server exited with {status}");
    }
}

#[test]
fn server_completes_initialize_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = ServerProcess::spawn(dir.path());

    let resp = server.initialize();
    let caps = &resp["result"]["capabilities"];
    assert!(caps["diagnosticProvider"].is_object());
    assert!(caps["codeActionProvider"].is_object());
    assert_eq!(caps["documentFormattingProvider"], json!(true));

    server.shutdown_and_exit();
}

#[test]
fn did_open_over_real_stdio_publishes_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = ServerProcess::spawn(dir.path());
    server.initialize();

    let file = dir.path().join("mod.py");
    let uri = file_uri(&file);
    send(
        &mut server.stdin,
        "textDocument/didOpen",
        None,
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "python",
                "version": 1,
                "text": "from os.path import join\n",
            }
        }),
    );

    let notif = read_notification(&server.reader, "textDocument/publishDiagnostics");
    let diagnostics = notif["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert_eq!(
        diagnostics.len(),
        1,
        "expected one KIS001 diagnostic: {notif}"
    );
    assert_eq!(diagnostics[0]["code"], json!("KIS001"));

    server.shutdown_and_exit();
}

#[test]
fn server_exits_promptly_after_shutdown_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = ServerProcess::spawn(dir.path());
    server.initialize();
    // Regression guard for the original stub test: the process must not
    // just "stay alive": it must actually terminate on shutdown/exit,
    // and do so quickly rather than hanging.
    let start = std::time::Instant::now();
    server.shutdown_and_exit();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "server took too long to exit after shutdown/exit"
    );
}

/// A client that cannot register file watchers (here: one that declares no
/// capabilities) still gets diagnostics refreshed when the config changes:
/// the server watches the config files itself.
#[test]
fn config_changes_are_picked_up_without_client_file_watching() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut server = ServerProcess::spawn(&root);
    let (_, reg) = server.initialize_with(json!({}));
    let methods = reg["params"]["registrations"].to_string();
    assert!(!methods.contains("didChangeWatchedFiles"), "{methods}");

    let uri = file_uri(&root.join("mod.py"));
    send(
        &mut server.stdin,
        "textDocument/didOpen",
        None,
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "python",
                "version": 1,
                "text": "from os.path import join\n",
            }
        }),
    );
    let notif = read_notification(&server.reader, "textDocument/publishDiagnostics");
    assert_eq!(notif["params"]["diagnostics"].as_array().unwrap().len(), 1);

    std::fs::write(
        root.join("pyproject.toml"),
        "[tool.konform.lint]\nignore = [\"KIS001\"]\n",
    )
    .unwrap();
    // A slow file system may report the file before its content is written,
    // so allow an intermediate reload before the final one.
    let cleared = (0..5).any(|_| {
        let notif = read_notification(&server.reader, "textDocument/publishDiagnostics");
        notif["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    });
    assert!(cleared, "KIS001 should be ignored after the config change");

    server.shutdown_and_exit();
}
