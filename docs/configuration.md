# Configuration

konform reads settings from `konform.toml` or `pyproject.toml`. Starting from the
directory being checked, it walks up the tree until it finds one (`konform.toml`
is preferred). Use `--config PATH` to point at a specific file, or `--isolated`
to ignore all configuration files.

=== "pyproject.toml"

    ```toml
    [tool.konform]
    cache-dir = ".konform_cache"
    src       = [".", "src"]

    [tool.konform.lint]
    select = ["KIS"]
    ignore = []
    level  = "error"

    [tool.konform.lint.module-only-imports]
    exceptions = ["__future__", "typing", "mycompany.compat"]
    ```

=== "konform.toml"

    In `konform.toml` the same tables are written under `[konform]` instead of `[tool.konform]`.

    ```toml
    [konform]
    cache-dir = ".konform_cache"
    src       = [".", "src"]

    [konform.lint]
    select = ["KIS"]
    ignore = []
    level  = "error"

    [konform.lint.module-only-imports]
    exceptions = ["__future__", "typing", "mycompany.compat"]
    ```

The complete list of options is in the [settings reference](reference/settings.md).
A config file that can't be read or parsed, or whose settings have the wrong type, is an
error: `check` and `rule --test` exit with code 2, other commands warn and the language
server shows it. konform never silently falls back to the defaults.
`konform check --show-settings .` prints the settings in effect, after `extend` and any
CLI flags, along with the config file used and the enabled rules.

## Per-rule tables

Each rule's settings live in a table keyed by a stable *config name* (shown by
`konform rule --list`), not by its code. Renaming a rule code (with
[`noqa-aliases`](suppression.md#aliasing-noqa-codes) covering old comments)
never forces a config rewrite.

## Sharing configuration

`extend` pulls in another config file and lays this one over it, so several projects can
share a base:

```toml
[tool.konform]
extend = "../shared/konform.toml"   # relative to this file; `~/` works too

[tool.konform.lint]
ignore = ["KIS002"]                 # replaces the base's `ignore`
```

The base is a `konform.toml` or a `pyproject.toml` and may `extend` another file in turn.
Tables (like `[tool.konform.lint.module-only-imports]`) merge key by key; any other value,
lists included, replaces the base's. Paths in the merged settings (`src`, `exclude`,
`per-file-ignores`, `rules_file`) stay relative to **this** project, not to the base
file. A missing base or an `extend` cycle is an error.

## Module search roots

[KIS001](rules/kis001.md) must decide whether an imported name is a module or an
attribute. It searches the filesystem starting from your Python environment's
`sys.path` plus extra roots, which matters for uninstalled local packages.

The extra roots resolve like Ruff's `src` setting:

1. `src = [...]` in `[tool.konform]`, if set.
2. Otherwise `[tool.ruff] src = [...]`.
3. Otherwise the default `[".", "src"]`.

Entries are relative to the directory containing the config file.

## Python environment

konform uses `python` if set, otherwise auto-discovers an interpreter, starting from
the directory containing the config file (or the working directory if there is no
config file). In order:

1. `.venv`, `venv` or `.env` in the config directory.
2. The same directories in each parent directory, up to and including the first one
   that contains `.git` (the repository root). If no `.git` is found, parents are not
   searched.
3. The active `VIRTUAL_ENV`, then `CONDA_PREFIX`.
4. `python3` on your `PATH` (`python.exe` on Windows).

Inside an environment the interpreter is `bin/python3` or `bin/python`; on Windows it
is `Scripts/python.exe`, or `python.exe` at the environment root (conda layout).

An explicit `python` always wins over discovery. `~` is expanded, and a relative path
is resolved against the directory containing the config file (previously it was
relative to the working directory). It may point above the repository root, which is
useful for monorepos where the venv lives above the config file:

```toml
[tool.konform]
python = "../.venv/bin/python"
```

> **Note:** konform executes this interpreter (the `python` setting or any
> auto-discovered venv interpreter) to read `sys.path` and the stdlib module list.
> Only lint projects you trust.

## Upgrading from an older config format

When the config shape changes (as in 0.3.0), konform auto-migrates an outdated
file in place the first time you run any command (comments and other
`[tool.*]` sections are preserved) and prints a summary to stderr. Files pulled in
through `extend` are migrated the same way; one that can't be written (say, a read-only
shared file) is upgraded in memory each run, with a note saying so.
