# Editors

konform ships a language server that shares the CLI's rule engine — no second
process and no stale results.

```console
$ konform server    # LSP over stdin/stdout
```

## Code actions

A quickfix is offered for every fixable diagnostic (including unsafe ones such as
KIS002 — editing one open document is an explicit, reviewable action), plus:

- **Fix all auto-fixable problems** — safe fixes only (`source.fixAll.konform`).
- **Fix all problems (including unsafe fixes)** (`source.fixAll.konform.unsafe`) —
  only offered when it changes something beyond the safe pass.

The server reloads rules when `pyproject.toml`, `konform.toml`,
`konform_patterns.toml`/`.yaml` or the configured `rules_file` changes. If you
point `rules_file` at a new path, restart the server.

## Neovim

```lua
vim.api.nvim_create_autocmd("FileType", {
  pattern = "python",
  callback = function()
    vim.lsp.start({
      name = "konform",
      cmd  = { "konform", "server" },
      root_dir = vim.fs.dirname(
        vim.fs.find({ "pyproject.toml", "konform.toml" }, { upward = true })[1]
      ),
    })
  end,
})
```

## VS Code

Use the generic [None ls](https://marketplace.visualstudio.com/items?itemName=esbenp.none-ls-vscode)
extension, or any client supporting a custom LSP command:

```json
{
  "nls.server": {
    "command": ["konform", "server"]
  }
}
```

## Zed

Install the `zed-konform` extension (see `extensions/zed-extension/README.md`);
Zed installs the `konform` binary automatically. To override:

```json
{
  "lsp": {
    "konform": {
      "binary": { "path": "konform", "arguments": ["server"] }
    }
  }
}
```
