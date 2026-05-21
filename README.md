# konform

Multi-rule Python linter and language server — fast, configurable, and CI-ready.

## Rules

### KIS001 — Google-style imports

Checks that every `from X import Y` only imports a sub-module, not an object
(function, class, or constant), following the
[Google Python Style Guide §2.2](https://google.github.io/styleguide/pyguide.html#22-imports).

```python
# Bad — KIS001: `join` is a function, not a module
from os.path import join

# Good
import os.path
from os import path       # `path` is a module
```

### KPT — User-defined pattern rules

Load regex patterns from `konform_patterns.toml` (auto-discovered next to
`pyproject.toml`) or inline in `pyproject.toml`:

```toml
[[tool.konform.KPT.rules]]
id      = "KPT001"
message = "Use the project logger instead of bare print()."
pattern = '^\s*print\s*\('
files   = ["src/**/*.py"]
level   = "warning"
```

## Installation

```bash
pip install konform
```

Wheels ship a pre-compiled Rust binary — no Rust installation needed at runtime.

## Usage

```bash
# Lint all Python files under src/
konform check src/

# Lint and apply auto-fixes in one pass
konform check --fix src/

# Apply fixes only (no lint report)
konform check --fix src/

# Show a unified diff of what format would change
konform check --diff src/

# Output violations as JSON (e.g. for tooling)
konform check --output-format json src/

# Suppress hints and summary (violations only)
konform check -q src/

# No output — just exit 1 on violations
konform check -s src/

# List all rules
konform rule --list

# Explain a rule
konform rule --explain KIS001

# Clear the local cache
konform clean
```

## Configuration

Add a `[tool.konform]` section to `pyproject.toml` (or a standalone `konform.toml`):

```toml
[tool.konform]
select    = []        # [] = all rules; prefix match: "KIS" = all KIS* rules
ignore    = []
level     = "error"   # "warning" | "error"
cache_dir = ".konform_cache"
workers   = 0         # 0 = os.cpu_count()

# ── KIS — import style ────────────────────────────────────────────────────
[tool.konform.KIS]
exceptions = [
    "__future__", "typing", "typing_extensions", "collections.abc",
    "mycompany.compat",
]
level = "error"

# ── KPT — user-defined patterns ───────────────────────────────────────────
[tool.konform.KPT]
level = "warning"
# Optional: load patterns from an external file instead of inline rules.
# rules_file = "konform_patterns.toml"

[[tool.konform.KPT.rules]]
id      = "KPT001"
message = "Use the project logger instead of bare print()."
pattern = '^\s*print\s*\('
files   = ["src/**/*.py"]
level   = "warning"
```

### Pattern files

Patterns can also live in a standalone `konform_patterns.toml` placed next to
`pyproject.toml`. konform auto-discovers it (no config key needed):

```toml
# konform_patterns.toml
[[rules]]
id      = "KPT002"
message = "Remove breakpoint() — debugging artefact."
pattern = '^\s*breakpoint\s*\(\s*\)'
level   = "error"
```

## Suppressing violations

```python
from os.path import join   # noqa: KIS001   ← exact rule
from os.path import join   # noqa: KIS       ← whole category
from os.path import join   # noqa             ← everything on this line
```

## Language Server (LSP)

konform ships a built-in LSP server that shares the same rule engine as the
CLI — no second process, no stale results.

```bash
konform server   # starts the LSP over stdin/stdout
```

### Neovim (nvim-lspconfig)

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

### VS Code (`settings.json`)

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

### Zed

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

## Development

```bash
# Compile the Rust binary and install it in the dev venv (required before tests)
hatch run develop

# Run tests with coverage
hatch test -c

# Build release wheels for all platforms
hatch run maturin:build-all
```

## CLI reference

```
konform check  [OPTIONS] <PATHS>…    Lint files (default subcommand)
konform check --fix-only [OPTIONS] <PATHS>…  Apply all auto-fixes in-place, exit 0
konform server                       Start the LSP server (stdin/stdout)
konform rule   --list                List all rules
konform rule   --explain <CODE>      Show full rule documentation
konform clean  [--config PATH]       Delete the cache directory
konform version                      Print konform's version

Global options (available on all subcommands):
  --color auto|always|never          Colour output control
  --isolated                         Ignore all config files
  -v / --verbose                     Extra output
  -q / --quiet                       Violations only (no summary/hints)
  -s / --silent                      No output; exit code only
```
