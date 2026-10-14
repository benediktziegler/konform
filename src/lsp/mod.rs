//! konform LSP server: entry point and public API.
//!
//! Invoked via `konform lsp` (detected in `main.rs` before clap).
//! Architecture: single-threaded main loop driven by `lsp-server`'s
//! crossbeam-based I/O threads.  No tokio: the rule engine is CPU-bound
//! and `rayon` handles any file-level parallelism.

pub mod convert;
pub mod handler;
pub mod session;

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
    let (root, notices) = resolve_root(&init_params, std::env::current_dir().unwrap_or_default());
    eprintln!("konform server: root {}", root.display());
    for notice in &notices {
        eprintln!("{notice}");
    }

    // ── Build the shared session ───────────────────────────────────────────
    let mut session = Session::start(root);
    session.notices = notices;
    let session = Arc::new(RwLock::new(session));

    eprintln!("konform server: initialized");

    // ── Main message loop ──────────────────────────────────────────────────
    handler::main_loop(connection, session);

    // ── Wait for I/O threads to finish ────────────────────────────────────
    io_threads.join().expect("I/O threads panicked");
    eprintln!("konform server: exiting");
}

/// The session root for an `initialize` request, plus messages for the user.
///
/// Uses the first workspace folder, else `rootUri`, else the deprecated
/// `rootPath`, else `cwd`. Only one root is supported: configuration is read
/// from it and applied to every document, so extra folders are reported.
fn resolve_root(init_params: &serde_json::Value, cwd: PathBuf) -> (PathBuf, Vec<String>) {
    let mut notices = Vec::new();
    let folders = init_params
        .get("workspaceFolders")
        .and_then(|f| f.as_array())
        .map_or(&[][..], Vec::as_slice);
    let root = match workspace_root(init_params) {
        Some(root) => root,
        None => {
            notices.push(format!(
                "konform: the editor gave no local workspace folder, so the working \
                 directory {} is used to find the configuration.",
                cwd.display()
            ));
            cwd
        }
    };
    if folders.len() > 1 {
        notices.push(format!(
            "konform: {} workspace folders are open, but konform uses the configuration of \
             the first one ({}) for all of them. Run one konform server per folder for \
             separate settings.",
            folders.len(),
            root.display()
        ));
    }
    (root, notices)
}

/// The local directory of the client's first usable root: its first
/// workspace folder, else `rootUri`, else the deprecated `rootPath`. `None`
/// when none is given or none is a local `file:` URI / path.
fn workspace_root(init_params: &serde_json::Value) -> Option<PathBuf> {
    let uri_of = |v: Option<&serde_json::Value>| v.and_then(|u| u.as_str()).map(str::to_owned);
    let first_folder = init_params
        .get("workspaceFolders")
        .and_then(|f| f.as_array())
        .and_then(|f| f.first())
        .and_then(|f| uri_of(f.get("uri")));
    [first_folder, uri_of(init_params.get("rootUri"))]
        .into_iter()
        .flatten()
        .find_map(|uri| local_dir(&uri))
        .or_else(|| {
            // `rootPath` is a plain path, deprecated in favour of `rootUri`.
            init_params
                .get("rootPath")
                .and_then(|p| p.as_str())
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
        })
}

/// The local path named by a `file:` URI, `None` for other schemes or a
/// `file://host/share` URI that names another machine.
fn local_dir(uri: &str) -> Option<PathBuf> {
    let uri = lsp_types::Uri::from_str(uri).ok()?;
    if !uri
        .scheme()
        .is_some_and(|s| s.as_str().eq_ignore_ascii_case("file"))
    {
        return None;
    }
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

    #[test]
    fn root_path_is_the_last_resort() {
        let params = json!({ "rootUri": "vscode-vfs://x/y", "rootPath": "/legacy" });
        assert_eq!(workspace_root(&params), Some(PathBuf::from("/legacy")));
        // A usable rootUri still wins, and so does a later local source over
        // a remote first folder.
        let params = json!({ "rootUri": "file:///new", "rootPath": "/legacy" });
        assert_eq!(workspace_root(&params), Some(PathBuf::from("/new")));
        let params = json!({
            "workspaceFolders": [{ "uri": "vscode-vfs://x/y", "name": "r" }],
            "rootUri": "file:///new",
        });
        assert_eq!(workspace_root(&params), Some(PathBuf::from("/new")));
        assert_eq!(workspace_root(&json!({ "rootPath": "" })), None);
    }

    #[test]
    fn a_missing_root_falls_back_to_cwd_with_a_notice() {
        let (root, notices) = resolve_root(&json!({}), PathBuf::from("/work"));
        assert_eq!(root, PathBuf::from("/work"));
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("/work"), "{notices:?}");

        let (root, notices) = resolve_root(&json!({ "rootUri": "file:///p" }), "/work".into());
        assert_eq!(root, PathBuf::from("/p"));
        assert!(notices.is_empty());
    }

    #[test]
    fn extra_workspace_folders_are_reported() {
        let params = json!({ "workspaceFolders": [
            { "uri": "file:///a", "name": "a" },
            { "uri": "file:///b", "name": "b" },
        ] });
        let (root, notices) = resolve_root(&params, PathBuf::from("/work"));
        assert_eq!(root, PathBuf::from("/a"));
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("2 workspace folders") && notices[0].contains("/a"),
            "{notices:?}"
        );
    }
}
