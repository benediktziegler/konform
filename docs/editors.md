---
title: Language server & editors
---

# Language Server (LSP)

konform ships a built-in LSP server that shares the same rule engine as the
CLI — no second process, no stale results.

```bash
konform server   # starts the LSP over stdin/stdout
```

Code actions mirror the CLI's fix-safety split: a per-violation quickfix is
offered for every fixable diagnostic (including unsafe ones, e.g. KIS002 —
editing a single open document is an explicit, reviewable action), plus two
document-wide "fix all" actions: **Fix all auto-fixable problems** (safe
fixes only, `source.fixAll.konform`) and **Fix all problems (including
unsafe fixes)** (`source.fixAll.konform.unsafe`), the latter only offered
when it would change something beyond the safe-only pass.

## Neovim (nvim-lspconfig)

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

## VS Code (`settings.json`)

Add via the generic
[`None ls`](https://marketplace.visualstudio.com/items?itemName=esbenp.none-ls-vscode)
or any client that supports a custom LSP command:

```json
{
  "nls.server": {
    "command": ["konform", "server"]
  }
}
```

## Zed

Install the `zed-konform` extension (see
`extensions/zed-extension/README.md`) and Zed will auto-install the
`konform` binary for you — no manual setup required. To override the binary
path or pass extra arguments, set:

```json
{
  "lsp": {
    "konform": {
      "binary": {
        "path": "konform",
        "arguments": ["server"]
      }
    }
  }
}
```
