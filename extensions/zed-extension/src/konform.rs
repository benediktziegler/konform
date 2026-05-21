//! Zed extension for the Konform Python linter and language server.
//!
//! Konform provides:
//! * **KIS001** — Google-style import checker (rewrite `from X import obj` → `import X`)
//! * **KPT**    — User-defined regex pattern rules loaded from `konform_patterns.toml`
//!
//! The extension locates the `konform` binary via `worktree.which("konform")` so
//! it works with any installation method (pip, pipx, hatch venv, …).  The binary
//! path can be overridden in Zed workspace settings:
//!
//! ```json
//! {
//!   "lsp": {
//!     "konform": {
//!       "binary": { "path": "/path/to/konform" }
//!     }
//!   }
//! }
//! ```

use zed_extension_api::{self as zed, settings::LspSettings, Command, LanguageServerId, Worktree};

struct KonformExtension;

impl zed::Extension for KonformExtension {
    fn new() -> Self {
        Self
    }

    /// Return the command that starts the konform language server.
    ///
    /// Resolution order:
    /// 1. `lsp.konform.binary.path` in Zed workspace/user settings
    /// 2. `konform` found on `$PATH` via the worktree shell environment
    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<Command> {
        let env = worktree.shell_env();

        // Allow the user to override the binary path and arguments via Zed settings.
        if let Ok(lsp_settings) = LspSettings::for_worktree("konform", worktree) {
            if let Some(binary) = lsp_settings.binary {
                if let Some(path) = binary.path {
                    let args = binary
                        .arguments
                        .unwrap_or_else(|| vec!["server".to_string()]);
                    return Ok(Command {
                        command: path,
                        args,
                        env,
                    });
                }
            }
        }

        // Fall back to locating the binary from the worktree's shell PATH.
        let path = worktree
            .which("konform")
            .ok_or_else(|| "konform must be installed and available in $PATH.  Run `pip install konform` or `pipx install konform`.".to_string())?;

        Ok(Command {
            command: path,
            args: vec!["server".to_string()],
            env,
        })
    }

    /// Forward `lsp.konform.initialization_options` from Zed settings to the server.
    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<Option<zed::serde_json::Value>> {
        let options = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.initialization_options.clone())
            .unwrap_or_default();
        Ok(Some(options))
    }

    /// Forward `lsp.konform.settings` from Zed settings to the server.
    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<Option<zed::serde_json::Value>> {
        let settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.settings.clone())
            .unwrap_or_default();
        Ok(Some(settings))
    }
}

zed::register_extension!(KonformExtension);
