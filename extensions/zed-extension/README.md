# zed-konform

Zed extension for the [Konform](https://github.com/benediktziegler/konform)
Python linter and language server.

## Features

* **Inline diagnostics** for every open `.py` file (push + pull, LSP 3.17)
* **KIS001** — flags `from X import obj` imports that should be `import X`
* **KPT** — user-defined regex pattern rules from `konform_patterns.toml`
* **Hover** — hover over a violation to read the full rule documentation
* **Code actions** — "Fix all konform violations" rewrites fixable imports in one shot
* **Auto-fix on save** via `textDocument/formatting`

## Installation

No prerequisites — the extension auto-installs `konform` for you.

From the Zed Extensions panel, click **Install Dev Extension** and select this
directory (or install the published extension from the Zed extension gallery).
Once compiled, Zed activates the extension for every `.py` file in your
workspace and resolves the `konform` binary in this order:

1. `lsp.konform.binary.path` in Zed settings, if set (see below).
2. `konform` on your `$PATH`, if you've already installed it via
   `uv tool install konform` / `pipx install konform`.
3. Otherwise, the extension downloads the standalone `konform` binary that
   matches your OS/architecture from the
   [GitHub releases](https://github.com/benediktziegler/konform/releases)
   and caches it alongside the extension — no Python or Rust toolchain
   required.

Prebuilt binaries are available for Linux (x86_64, aarch64, glibc), macOS
(x86_64, aarch64) and Windows (x86_64). On other platforms, install `konform`
via `uv`/`pipx` so it can be found on `$PATH`.

Verify a manual install with:

```bash
konform version
```

## Configuration

Override the binary path or pass extra arguments in your Zed workspace
settings (`~/.config/zed/settings.json`):

```json
{
  "lsp": {
    "konform": {
      "binary": {
        "path": "/home/you/.venv/bin/konform",
        "arguments": ["server"]
      }
    }
  }
}
```

## Building

Zed builds the extension automatically when you install it as a dev extension.
To build manually (requires `rustup` and the `wasm32-wasip1` target):

```bash
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release
```
