//! konform LSP server: entry point and public API.
//!
//! Invoked via `konform lsp` (detected in `main.rs` before clap).
//! Architecture: single-threaded main loop driven by `lsp-server`'s
//! crossbeam-based I/O threads.  No tokio: the rule engine is CPU-bound
//! and `rayon` handles any file-level parallelism.

pub mod convert;
pub mod handler;
pub mod session;

use crate::config::{load_config, resolve_python};
use crate::module_probe::ModuleProbe;
use session::Session;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

/// Start the language server (reads from stdin, writes to stdout).
///
/// This function blocks until the client sends `shutdown` + `exit`.
pub fn run() {
    eprintln!("konform server: starting");

    // ── Open the stdio connection ──────────────────────────────────────────
    let (connection, io_threads) = lsp_server::Connection::stdio();

    // ── LSP initialization handshake ──────────────────────────────────────
    let server_caps = handler::server_capabilities();
    let caps_value =
        serde_json::to_value(server_caps).expect("ServerCapabilities must be serializable");
    let init_params = connection
        .initialize(caps_value)
        .expect("initialization handshake failed");

    // ── Load config from the workspace root, else the working directory ───
    let root =
        workspace_root(&init_params).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    eprintln!("konform server: root {}", root.display());
    let config = load_config(Some(&root), None);
    let python = resolve_python(&config);
    let module_probe_root = config.config_dir.clone().unwrap_or_else(|| root.clone());
    let probe = Arc::new(ModuleProbe::new(&python, &module_probe_root, &config.src));

    // ── Build the shared session ───────────────────────────────────────────
    let session = Arc::new(RwLock::new(Session::new(config, probe, root)));

    eprintln!("konform server: initialized");

    // ── Main message loop ──────────────────────────────────────────────────
    handler::main_loop(connection, session);

    // ── Wait for I/O threads to finish ────────────────────────────────────
    io_threads.join().expect("I/O threads panicked");
    eprintln!("konform server: exiting");
}

/// The local directory of the client's first workspace folder, else its
/// `rootUri`. `None` when neither is given or it is not a local `file:` URI.
fn workspace_root(init_params: &serde_json::Value) -> Option<PathBuf> {
    let from_folders = init_params
        .get("workspaceFolders")
        .and_then(|f| f.as_array())
        .and_then(|f| f.first())
        .and_then(|f| f.get("uri"))
        .and_then(|u| u.as_str());
    let uri = from_folders.or_else(|| init_params.get("rootUri").and_then(|u| u.as_str()))?;
    let uri = lsp_types::Uri::from_str(uri).ok()?;
    if !uri
        .scheme()
        .is_some_and(|s| s.as_str().eq_ignore_ascii_case("file"))
    {
        return None;
    }
    // Only a local path: a `file://host/share` URI names another machine.
    if uri
        .authority()
        .is_some_and(|a| !a.as_str().is_empty() && !a.as_str().eq_ignore_ascii_case("localhost"))
    {
        return None;
    }
    Some(session::uri_file_path(&uri))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_comes_from_the_first_workspace_folder() {
        let params = json!({
            "rootUri": "file:///old",
            "workspaceFolders": [
                { "uri": "file:///my%20dir/proj", "name": "p" },
                { "uri": "file:///second", "name": "s" },
            ],
        });
        assert_eq!(workspace_root(&params), Some(PathBuf::from("/my dir/proj")));
    }

    #[test]
    fn root_falls_back_to_root_uri() {
        let params = json!({ "rootUri": "file:///proj", "workspaceFolders": [] });
        assert_eq!(workspace_root(&params), Some(PathBuf::from("/proj")));
        let params = json!({ "rootUri": "file://localhost/proj" });
        assert_eq!(workspace_root(&params), Some(PathBuf::from("/proj")));
        // The host is case-insensitive.
        let params = json!({ "rootUri": "file://LocalHost/proj" });
        assert_eq!(workspace_root(&params), Some(PathBuf::from("/proj")));
    }

    #[test]
    fn non_local_or_missing_roots_are_ignored() {
        assert_eq!(workspace_root(&json!({})), None);
        assert_eq!(workspace_root(&json!({ "rootUri": null })), None);
        assert_eq!(
            workspace_root(&json!({ "rootUri": "vscode-vfs://x/y" })),
            None
        );
        assert_eq!(
            workspace_root(&json!({ "rootUri": "file://server/share" })),
            None
        );
        assert_eq!(workspace_root(&json!({ "rootUri": "not a uri" })), None);
    }
}
