# konform

Multi-rule Python linter and language server — fast, configurable, and CI-ready.

## Rules

### KIS001 — Module-only imports

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

This rule is sometimes fixable: konform rewrites the import automatically
when it's safe to do so. It leaves the violation for you to fix by hand when
the new import's name is already bound elsewhere in the file -- either as a
local variable, or by a different import that would then overlap with it
(two `from X import Y` statements silently bound to the same name but
pointing at different modules).

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
cache-dir = ".konform_cache"
workers   = 0         # 0 = os.cpu_count()
src       = [".", "src"]   # search roots for KIS001's module-existence probe;
                            # see "Module search roots" below.

[tool.konform.lint]
select = []        # [] = all rules; prefix match: "KIS" = all KIS* rules
ignore = []
level  = "error"   # "warning" | "error"

# ── KIS001 — import style ──────────────────────────────────────────────────
[tool.konform.lint.module-only-imports]
exceptions = [
    "__future__", "typing", "typing_extensions", "collections.abc",
    "mycompany.compat",
]
level = "error"
unresolved-level = "warning"   # "warning" (default) | "error" | "off"
                                # Used when a package isn't installed in this
                                # environment, so KIS001 can't tell whether the
                                # imported name is a module or not.

# ── KPT001 — user-defined patterns ─────────────────────────────────────────
[tool.konform.lint.user-defined-patterns]
level = "warning"
# Optional: load patterns from an external file instead of inline rules.
# rules_file = "konform_patterns.toml"

[[tool.konform.lint.user-defined-patterns.rules]]
id      = "KPT001"
message = "Use the project logger instead of bare print()."
pattern = '^\s*print\s*\('
files   = ["src/**/*.py"]
level   = "warning"

# Sub-rules are attached to this rule entry. This inline form makes the
# parent/child relation explicit and avoids table-order confusion.
sub_rules = [
  {
    pattern = ['print\(.*password', 'print\(.*secret'],
    message = "Never print credentials.",
    help = "Use redaction helpers before logging.",
  },
]
```

Each rule's settings live in its own table, keyed by a stable config name
(shown by `konform rule --list`) rather than by rule code — so renaming a
rule code (with `noqa-aliases` covering old suppression comments) never
forces you to also rewrite your config.

When using `[[tool.konform.lint.user-defined-patterns.rules.sub_rules]]`, TOML
binds each sub-rule to _the most recently declared_
`[[tool.konform.lint.user-defined-patterns.rules]]` entry.

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

### Module search roots

KIS001 needs to know whether an imported name is a real module (`import os.path`)
or just an attribute of one (`from os.path import join`). It answers this by
searching the filesystem, starting from your Python environment's `sys.path`
plus a configurable set of extra roots -- this matters for local packages that
aren't installed (e.g. a `src/` layout, or code laid out some other way).

The extra roots are resolved the same way as Ruff's `src` setting, including
its precedence:

1. `[tool.konform] src = [...]`, if set.
2. Otherwise, `[tool.ruff] src = [...]`, if your project already configures
   Ruff for a non-standard layout.
3. Otherwise, the default `[".", "src"]` (covers both flat and `src` layouts
   out of the box).

Each entry is resolved relative to the directory containing `pyproject.toml`
/ `konform.toml`. For example, if your package lives under `lib/`:

```toml
[tool.konform]
src = ["lib"]
```

## Suppressing violations

```python
from os.path import join   # noqa: KIS001   ← exact rule
from os.path import join   # noqa: KIS       ← whole category
from os.path import join   # noqa             ← everything on this line
```

### Aliasing noqa codes

When a rule code changes (e.g. a rule is renamed, or a project migrates
from another linter's codes), old `# noqa` comments would otherwise stop
working. Define aliases in your config so they keep suppressing the
renamed/canonical rule:

```toml
# pyproject.toml
[tool.konform.lint.noqa-aliases]
IS001 = "KIS001"
IS    = "KIS"
```

```python
from os.path import join   # noqa: IS001   ← suppresses KIS001 via alias
```

### Upgrading from an older config format

When konform's config shape changes (as it did in 0.3.0, moving rule
selection/settings under `[tool.konform.lint]`), it auto-migrates an
outdated `pyproject.toml` / `konform.toml` the first time you run any
konform command: the file is rewritten in place (comments and other
`[tool.*]` sections are preserved) and a summary of what changed is printed
to stderr. No action is needed beyond re-running konform once.

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
