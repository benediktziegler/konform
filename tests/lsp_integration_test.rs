//! End-to-end LSP smoke test over **real stdio**, against the compiled
//! binary — mirroring the blackbox approach used by tools like `pytest-lsp`.
//!
//! This intentionally duplicates only a thin slice of what
//! `src/lsp/handler/tests.rs`'s in-process harness already covers in depth.
//! Its job is narrower: catch bugs the in-process harness *can't* see —
//! stdio framing, process startup/argv handling, and clean shutdown/exit —
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

/// Read messages until one whose `method` matches `method`, discarding
/// anything else (e.g. server-initiated `client/registerCapability`).
fn read_notification(reader: &mut impl BufRead, method: &str) -> Value {
    for _ in 0..20 {
        let msg = read_message(reader);
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
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
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
        let reader = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self {
            child,
            stdin,
            reader,
        }
    }

    fn initialize(&mut self) -> Value {
        send(
            &mut self.stdin,
            "initialize",
            Some(1),
            json!({ "processId": null, "rootUri": null, "capabilities": {} }),
        );
        let resp = read_message(&mut self.reader);
        assert_eq!(resp["id"], 1, "unexpected response id: {resp}");
        send(&mut self.stdin, "initialized", None, json!({}));
        resp
    }

    fn shutdown_and_exit(mut self) {
        send(&mut self.stdin, "shutdown", Some(999), Value::Null);
        let resp = read_message(&mut self.reader);
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

    let notif = read_notification(&mut server.reader, "textDocument/publishDiagnostics");
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
    // just "stay alive" — it must actually terminate on shutdown/exit,
    // and do so quickly rather than hanging.
    let start = std::time::Instant::now();
    server.shutdown_and_exit();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "server took too long to exit after shutdown/exit"
    );
}
