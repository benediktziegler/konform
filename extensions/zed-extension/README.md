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

## Prerequisites

`konform` must be on your `$PATH`:

```bash
pip install konform
# or
pipx install konform
```

Verify with:

```bash
konform version
```

## Installing the extension

From the Zed Extensions panel, click **Install Dev Extension** and select this
directory.  Once compiled, Zed will activate the extension for every `.py`
file in your workspace.

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
