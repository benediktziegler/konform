//! Zed extension for the Konform Python linter and language server.
//!
//! Konform provides:
//! * **KIS001** — Module-only import checker (rewrite `from X import obj` → `import X`)
//! * **KPT**    — User-defined regex pattern rules loaded from `konform_patterns.toml`
//!
//! Binary resolution order:
//! 1. `lsp.konform.binary.path` in Zed workspace/user settings (explicit override).
//! 2. `konform` found on `$PATH` via the worktree shell environment (pip/pipx/venv installs).
//! 3. Auto-installed: the extension downloads the standalone `konform` binary that
//!    matches the current OS/architecture from the project's GitHub releases and
//!    caches it in the extension's work directory. No `pip install` required.
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

use std::fs;

use zed_extension_api::{
    self as zed, settings::LspSettings, Architecture, Command, DownloadedFileType,
    GithubReleaseOptions, LanguageServerId, LanguageServerInstallationStatus, Os, Worktree,
};

const GITHUB_REPO: &str = "benediktziegler/konform";

#[derive(Default)]
struct KonformExtension {
    cached_binary_path: Option<String>,
}

impl KonformExtension {
    /// Resolve the `konform` binary, auto-installing it from GitHub releases if
    /// it isn't already on `$PATH` (and isn't overridden via settings).
    fn language_server_binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<String> {
        if let Some(path) = worktree.which("konform") {
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).is_ok() {
                return Ok(path.clone());
            }
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = zed::latest_github_release(
            GITHUB_REPO,
            GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (os, arch) = zed::current_platform();
        let asset_name = match (os, arch) {
            (Os::Linux, Architecture::X8664) => "konform-x86_64-unknown-linux-gnu",
            (Os::Linux, Architecture::Aarch64) => "konform-aarch64-unknown-linux-gnu",
            (Os::Mac, Architecture::X8664) => "konform-x86_64-apple-darwin",
            (Os::Mac, Architecture::Aarch64) => "konform-aarch64-apple-darwin",
            (Os::Windows, Architecture::X8664) => "konform-x86_64-pc-windows-msvc.exe",
            _ => {
                return Err(
                    "konform has no prebuilt binary for this platform. Install it with `pip install konform` or `pipx install konform` and it will be picked up from $PATH."
                        .to_string(),
                )
            }
        };

        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "no asset named {asset_name} found in konform release {}",
                    release.version
                )
            })?;

        let version_dir = format!("konform-{}", release.version);
        let binary_path = format!(
            "{version_dir}/{}",
            if os == Os::Windows {
                "konform.exe"
            } else {
                "konform"
            }
        );

        if fs::metadata(&binary_path).is_err() {
            zed::set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::Downloading,
            );

            fs::create_dir_all(&version_dir)
                .map_err(|e| format!("failed to create directory {version_dir}: {e}"))?;
            zed::download_file(
                &asset.download_url,
                &binary_path,
                DownloadedFileType::Uncompressed,
            )
            .map_err(|e| format!("failed to download {asset_name}: {e}"))?;
            zed::make_file_executable(&binary_path)?;

            // Clean up older cached versions.
            if let Ok(entries) = fs::read_dir(".") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("konform-") && name != version_dir {
                        fs::remove_dir_all(entry.path()).ok();
                    }
                }
            }
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

impl zed::Extension for KonformExtension {
    fn new() -> Self {
        Self::default()
    }

    /// Return the command that starts the konform language server.
    ///
    /// Resolution order:
    /// 1. `lsp.konform.binary.path` in Zed workspace/user settings
    /// 2. `konform` found on `$PATH` via the worktree shell environment
    /// 3. Auto-installed standalone binary downloaded from GitHub releases
    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
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

        let path = self.language_server_binary_path(language_server_id, worktree)?;

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
