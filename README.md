# konform

Multi-rule Python linter and language server — fast, configurable, and CI-ready.

> **Work in progress.** konform is under active development. Rules,
> configuration keys, CLI flags, and the LSP surface may change at any time,
> including in backwards-incompatible ways, until a 1.0 release.

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

### KIS002 — Import alias policy

Flags `from X import Y as Z` when the alias `Z` buys nothing -- `Y` isn't
bound to anything else in the module, so the alias only adds a layer of
indirection.

**Why:** an alias that isn't needed gives one thing two names. Readers have
to learn that `bar_baz` is really `foo.bar.baz`, grepping for `baz` misses
its uses, and the same import can end up aliased differently from file to
file. Aliases earn their place when they resolve a real name clash; this rule
flags the ones that don't.

```python
# Bad — KIS002: `bar_baz` isn't needed, nothing else is named `baz`
from foo.bar import baz as bar_baz

bar_baz()

# Good
from foo.bar import baz

baz()
```

#### What the fix does

With `--fix --unsafe-fixes`, konform drops the alias and renames every use of
it back to the original name:

```python
# Before
from foo.bar import baz as bar_baz

def run():
    return bar_baz()

# After
from foo.bar import baz

def run():
    return baz()
```

#### When an alias is legitimate (not flagged)

```python
# The alias avoids a clash with a local name or another import
from foo.bar import baz as bar_baz
baz = compute()                 # `baz` is already taken here

# Several imports of the same name from different modules: the aliases keep
# them apart
from foo.bar import baz as bar_baz
from nor.kind import baz as kind_baz

# The alias is a deliberate re-export under that name
__all__ = ["bar_baz"]
from foo.bar import baz as bar_baz

# Leading underscore: a "private, don't re-export" marker
from foo.bar import baz as _baz
```

The alias is also left alone for a self-alias (`from X import Y as Y`, which
is Ruff's `PLC0414` territory) and for relative imports
(`from . import x as y`, which have no stable module identity to key a
collision check on). Plain `import X as Z` statements are out of scope too --
dropping the alias there changes what gets bound, unlike
`from X import Y as Z`.

#### Enforcing one aliasing convention

If your project deliberately aliases some imports (say, always
`<last module part>_<name>`), set `alias-template` and aliases matching it
are never flagged, even when the rename isn't needed:

```toml
[tool.konform.lint.import-alias-policy]
alias-template = "{module_last}_{name}"
```

```python
from foo.bar import baz as bar_baz    # OK: matches the template
from foo.bar import baz as other      # KIS002: doesn't match, and isn't needed
```

Placeholders are `{name}`, `{module}` (dots become `_`), `{module_first}` and
`{module_last}`. The template only exempts aliases; it never flags any. An
invalid template is reported on stderr and ignored.

This rule is sometimes fixable: konform drops the alias and renames every use
of it back to the original name. It leaves the violation for you to fix by
hand when the alias is shadowed somewhere, or bound to more than one import
in the file.

The fix is marked **unsafe** -- konform can't rule out other, dynamic
references to the alias by name, so it's only applied with `--unsafe-fixes`
(see [Usage](#usage) below).

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

Each pattern is addressable by its own `id` in `select`, `ignore`,
`per-file-ignores` and `# noqa`. `ignore = ["KPT001"]` silences only that
pattern; `ignore = ["KPT"]` silences every pattern.

### KST — User-defined structural rules

KPT matches text; KST matches **code structure** (the syntax tree), so a rule
keeps working whatever the formatting, comments or import aliases. For
example, forbid `assert` inside pytest fixtures:

```toml
[[tool.konform.lint.structural-rules.rules]]
id      = "KST001"
message = "Do not use assert inside a pytest fixture."
help    = "Raise an explicit exception instead."
level   = "error"
files   = ["tests/**"]
match   = { kind = "assert", inside = { decorated_with = "pytest.fixture" } }
```

```python
import pytest


@pytest.fixture
def my_fixture():
    a = 1
    assert a == 3  # error[KST001]
```

Rules can also live in `konform_rules.toml` next to `pyproject.toml`
(auto-discovered, `[[rules]]` tables) or in a file named by
`rules_file = "…"` (`.toml` or `.yaml`). Ids must start with `KST`; each id
is addressable in `select`, `ignore`, `per-file-ignores` and `# noqa`.

A `match` is a table whose conditions must **all** hold:

| Key              | Meaning                                                              |
| ---------------- | -------------------------------------------------------------------- |
| `kind`           | node kind, or a list (any of): `function` `class` `lambda` `assert` `assign` `import` `return` `raise` `yield` `try` `with` `for` `while` `if` `call` `await` `name` `attribute` |
| `name`           | regex searched in the node's identifier (anchor it with `^…$`)       |
| `qualname`       | import-resolved dotted name of a `name` / `attribute` node           |
| `callee`         | import-resolved dotted name of a call's function                     |
| `decorated_with` | import-resolved decorator name(s) on a function or class             |
| `inside`         | some ancestor matches                                                |
| `has`            | some descendant matches                                              |
| `not`            | the node does not match                                              |
| `all` / `any`    | lists of matchers; all / at least one must match                     |

`inside` and `has` take an extra `stop_by` matcher that ends the search. The
node matching it is still tried first, nothing beyond it is. To ignore
helper functions nested in a fixture:

```toml
match = { kind = "assert", inside = { decorated_with = "pytest.fixture", stop_by = { kind = "function" } } }
```

Names resolve through the file's imports: `@fixture` after
`from pytest import fixture`, `@pt.fixture` after `import pytest as pt` and
`@pytest.fixture(scope="session")` all count as `pytest.fixture`. Simple alias
assignments (`fx = pytest.fixture`) resolve the same way; they are ignored
for any name that is also assigned something else in the file, and a call
result (`fx = pytest.fixture(scope="session")`) is a value, not an alias.
Resolution
is file-wide and ignores local rebinding. Violations are reported at the
matched node (the name for functions and classes, the first line for other
blocks). An invalid rule is a hard error: `konform check` prints
every problem (`error: <source>: rule 'KST002': <why>`) and exits 2 without
linting, so a typo cannot produce a false green in CI. The language server
instead shows one warning (`window/showMessage`) and keeps running the valid
rules. KST rules are not auto-fixable. `konform rule --explain KST000` documents
the format.

**Writing rules:** `konform ast FILE` prints the tree exactly as KST sees it.
Each line is a node you can match with `kind`, with its position and the
values the conditions compare against (`name`, import-resolved `qualname` /
`callee`, `decorators`):

```text
$ konform ast tests/conftest.py
import 1:1
function 5:5  name=my_fixture  decorators=[pytest.fixture]
  assert 8:5
```

**Testing rules:** add `valid` / `invalid` snippets to a rule and run
`konform rule --test`. Each `valid` snippet must produce no violation of that
rule, each `invalid` one at least one (the rule's `files` glob is ignored for
snippets). Exit code 1 if a snippet fails, 2 if a rule is invalid; handy in
CI next to `konform check`.

```toml
[[rules]]
id      = "KST001"
message = "No `assert` in pytest fixtures"
match   = { kind = "assert", inside = { kind = "function", decorated_with = "pytest.fixture" } }

[rules.test]
valid   = ["import pytest\n@pytest.fixture\ndef f():\n    return 1\n"]
invalid = ["import pytest\n@pytest.fixture\ndef f():\n    assert 1\n"]
```

The node kinds and condition keys above are **konform's own vocabulary**, not
the parser's: each kind maps onto one or more parser node types inside
konform (for example `assign` covers `=`, annotated and augmented
assignment; `function` covers `def` and `async def`; `yield` covers
`yield from`). Rules therefore stay valid when the underlying parser is
upgraded or replaced, and parser type names (such as Ruff's `StmtAssert`)
are rejected as unknown kinds.

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

# Also apply fixes marked unsafe (e.g. KIS002)
konform check --fix --unsafe-fixes src/

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

# List all rules (including the user-defined KPT patterns of this project)
konform rule --list

# Explain a rule or a user-defined pattern id
konform rule --explain KIS001
konform rule --explain KPT030

# Clear the local cache
konform clean
```

When some reported violations are fixable only via `--unsafe-fixes` (e.g.
KIS002), the summary breaks safe and unsafe-only counts out separately, and
the suggested fix command is adjusted accordingly.

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

# ── KIS002 — import alias policy ──────────────────────────────────────────
[tool.konform.lint.import-alias-policy]
level = "warning"
# Aliased imports of the same name from different modules in one scope are
# never flagged (the aliases keep them apart).
# Optional: also always allow aliases that match this template, even when the
# rename isn't needed (it only exempts aliases, never flags them).
# Placeholders: {name}, {module}, {module_first}, {module_last}; no other
# braces. An invalid template is reported on stderr and ignored.
# alias-template = "{module_last}_{name}"   # foo.bar -> baz as bar_baz

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

# ── KST — user-defined structural rules ────────────────────────────────────
# Rules match the syntax tree; see "KST — User-defined structural rules" above.
[tool.konform.lint.structural-rules]
level = "warning"
# rules_file = "konform_rules.toml"

[[tool.konform.lint.structural-rules.rules]]
id      = "KST001"
message = "Do not use assert inside a pytest fixture."
match   = { kind = "assert", inside = { decorated_with = "pytest.fixture" } }
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

## Caching

Results are cached per file (keyed by mtime and permissions) in `cache-dir`.
The cache is also keyed by the settings that affect results — `select`,
`ignore`, the Python environment, rule config tables, `per-file-ignores`,
`noqa-aliases` and the content of user-defined patterns and structural rules
(including `konform_patterns.toml` and `konform_rules.toml`) — so editing any
of them re-lints unchanged files.
Use `--no-cache` to bypass it, or `konform clean` to delete it.

## Suppressing violations

```python
from os.path import join   # noqa: KIS001   ← exact rule
from os.path import join   # noqa: KIS       ← whole category
from os.path import join   # noqa             ← everything on this line
```

Multiple codes are comma-separated (`# noqa: KIS001, KPT010`); empty entries
(e.g. a trailing comma) are ignored, and `# noqa:` with no codes behaves like
a bare `# noqa`. In Python files, only real comments count: the text `# noqa`
inside a string literal does not suppress anything.

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

Code actions mirror the CLI's fix-safety split: a per-violation quickfix is
offered for every fixable diagnostic (including unsafe ones, e.g. KIS002 —
editing a single open document is an explicit, reviewable action), plus two
document-wide "fix all" actions: **Fix all auto-fixable problems** (safe
fixes only, `source.fixAll.konform`) and **Fix all problems (including
unsafe fixes)** (`source.fixAll.konform.unsafe`), the latter only offered
when it would change something beyond the safe-only pass.

The server builds its rules once and reloads them when `pyproject.toml`,
`konform.toml`, `konform_patterns.toml` / `.yaml`, `konform_rules.toml`, or the
configured `rules_file` (of either rule) changes (it asks the editor to watch
those files). If you point `rules_file` at a different path, restart the server
so the new file is watched.

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

## Acknowledgements

konform exists because [Ruff](https://github.com/astral-sh/ruff) doesn't (yet)
cover rules like KIS001/KIS002 — konform was built to fill that gap, and
Ruff's design was a direct inspiration for how konform is configured, how it
reports violations, and how it resolves `src` search roots.

konform is also built on top of several excellent open-source projects, especially:

- [`ruff_python_parser`](https://crates.io/crates/ruff_python_parser),
  [`ruff_python_ast`](https://crates.io/crates/ruff_python_ast), and
  [`ruff_text_size`](https://crates.io/crates/ruff_text_size) — Ruff's own
  Python parser and AST, published standalone on crates.io.

Thanks to all the maintainers of all used open-source projects.
