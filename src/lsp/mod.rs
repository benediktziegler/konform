//! konform LSP server — entry point and public API.
//!
//! Invoked via `konform lsp` (detected in `main.rs` before clap).
//! Architecture: single-threaded main loop driven by `lsp-server`'s
//! crossbeam-based I/O threads.  No tokio — the rule engine is CPU-bound
//! and `rayon` handles any file-level parallelism.

pub mod convert;
pub mod handler;
pub mod session;

use crate::config::{load_config, resolve_python};
use crate::module_probe::ModuleProbe;
use session::Session;
use std::sync::{Arc, RwLock};

/// Start the language server (reads from stdin, writes to stdout).
///
/// This function blocks until the client sends `shutdown` + `exit`.
pub fn run() {
    eprintln!("konform server: starting");

    // ── Load config from the process working directory ─────────────────────
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = load_config(Some(&cwd), None);
    let python = resolve_python(&config);
    let probe = Arc::new(ModuleProbe::new(&python));

    // ── Build the shared session ───────────────────────────────────────────
    let session = Arc::new(RwLock::new(Session::new(config, probe, cwd)));

    // ── Open the stdio connection ──────────────────────────────────────────
    let (connection, io_threads) = lsp_server::Connection::stdio();

    // ── LSP initialization handshake ──────────────────────────────────────
    let server_caps = handler::server_capabilities();
    let caps_value =
        serde_json::to_value(server_caps).expect("ServerCapabilities must be serializable");
    let _init_params = connection
        .initialize(caps_value)
        .expect("initialization handshake failed");

    eprintln!("konform server: initialized");

    // ── Main message loop ──────────────────────────────────────────────────
    handler::main_loop(connection, session);

    // ── Wait for I/O threads to finish ────────────────────────────────────
    io_threads.join().expect("I/O threads panicked");
    eprintln!("konform server: exiting");
}
