# Settings

All settings live in `konform.toml` (under `[konform]`) or `pyproject.toml` (under
`[tool.konform]`). Tables below use the `pyproject.toml` form. See
[Configuration](../configuration.md) for file discovery.

## Top-level — `[tool.konform]`

#### `cache-dir`
Directory for the [result cache](../caching.md).

**Default:** `".konform_cache"` · **Type:** `str`

#### `workers`
Number of worker threads. `0` means `os.cpu_count()`.

**Default:** `0` · **Type:** `int`

#### `python`
Python interpreter used to resolve imports. If unset, `.venv`, `venv` and `.env` are
auto-discovered.

**Default:** auto-discovered · **Type:** `str`

#### `src`
Extra module search roots for [KIS001](../rules/kis001.md), relative to the config
file. Falls back to `[tool.ruff] src`, then `[".", "src"]`.

**Default:** `[".", "src"]` · **Type:** `list[str]`

```toml
[tool.konform]
src = ["lib"]
```

## Linter — `[tool.konform.lint]`

#### `select`
Rule codes or prefixes to enable. Empty means all rules.

**Default:** `[]` · **Type:** `list[str]` · **CLI:** `--select`, `--extend-select`

#### `ignore`
Rule codes or prefixes to disable (prefix-matched).

**Default:** `[]` · **Type:** `list[str]` · **CLI:** `--ignore`, `--extend-ignore`

#### `level`
Minimum severity that causes a non-zero exit.

**Default:** `"error"` · **Type:** `"warning" | "error"` · **CLI:** `--level`

#### `per-file-ignores`
Map of glob (relative to the project root) to codes to ignore for matching files.

**Default:** `{}` · **Type:** `dict[str, list[str]]` · **CLI:** `--per-file-ignores`, `--extend-per-file-ignores`

```toml
[tool.konform.lint.per-file-ignores]
"tests/**" = ["KIS001", "KPT"]
```

#### `noqa-aliases`
Map of alternate code to canonical code so old `# noqa` comments keep working.
See [Suppressing violations](../suppression.md#aliasing-noqa-codes).

**Default:** `{}` · **Type:** `dict[str, str]`

## Rule tables

Keyed by config name under `[tool.konform.lint]`.

### `module-only-imports` ([KIS001](../rules/kis001.md))

| Option             | Type                            | Default                                                        |
| ------------------ | ------------------------------- | -------------------------------------------------------------- |
| `exceptions`       | `list[str]`                     | `__future__`, `typing`, `typing_extensions`, `collections.abc` |
| `level`            | `"warning" \| "error"`          | `"error"`                                                      |
| `unresolved-level` | `"warning" \| "error" \| "off"` | `"warning"`                                                    |

### `import-alias-policy` ([KIS002](../rules/kis002.md))

| Option           | Type                   | Default     |
| ---------------- | ---------------------- | ----------- |
| `level`          | `"warning" \| "error"` | `"warning"` |
| `alias-template` | `str`                  | unset       |

### `user-defined-patterns` ([KPT](../rules/kpt.md))

| Option       | Type                   | Default     |
| ------------ | ---------------------- | ----------- |
| `level`      | `"warning" \| "error"` | `"warning"` |
| `rules_file` | `str`                  | unset       |
| `rules`      | array of tables        | `[]`        |

### `structural-rules` ([KST](../rules/kst.md))

| Option       | Type                   | Default     |
| ------------ | ---------------------- | ----------- |
| `level`      | `"warning" \| "error"` | `"warning"` |
| `rules_file` | `str`                  | unset       |
| `rules`      | array of tables        | `[]`        |

## Full example

```toml
[tool.konform]
cache-dir = ".konform_cache"
workers   = 0
src       = [".", "src"]

[tool.konform.lint]
select = []
ignore = []
level  = "error"

[tool.konform.lint.module-only-imports]
exceptions = ["__future__", "typing", "typing_extensions", "collections.abc", "mycompany.compat"]
level = "error"
unresolved-level = "warning"

[tool.konform.lint.import-alias-policy]
level = "warning"
# alias-template = "{module_last}_{name}"

[tool.konform.lint.user-defined-patterns]
level = "warning"
# rules_file = "konform_patterns.toml"

[[tool.konform.lint.user-defined-patterns.rules]]
id      = "KPT001"
message = "Use the project logger instead of bare print()."
pattern = '^\s*print\s*\('
files   = ["src/**/*.py"]
level   = "warning"
```
