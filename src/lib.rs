//! konform as a library.
//!
//! The `konform` binary is a thin CLI over these modules. They are public so
//! the benchmarks in `benches/` can drive the engine directly; they are not a
//! stable API.

pub mod cache;
pub mod cli;
pub mod cli_docs;
pub mod config;
pub mod engine;
pub mod git;
pub mod lsp;
pub mod markdown;
pub mod migrations;
pub mod module_probe;
pub mod output;
pub mod rules;
pub mod theme;
pub mod types;
