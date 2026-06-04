use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[test]
fn test_lsp_server_starts() {
    // We use cargo run to ensure the binary is built and available.
    let mut child = Command::new("cargo")
        .arg("run")
        .arg("--bin")
        .arg("konform")
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn lsp server");

    // In a real LSP test, we'd send an 'initialize' request here via stdin.
    // For now, let's just check if it stays alive for a moment.
    thread::sleep(Duration::from_millis(500));

    assert!(
        child.try_wait().unwrap().is_none(),
        "LSP server exited prematurely"
    );

    // Kill it
    let _ = child.kill();
}
