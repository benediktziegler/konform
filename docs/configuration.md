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

## Per-rule tables

Each rule's settings live in a table keyed by a stable *config name* (shown by
`konform rule --list`), not by its code. Renaming a rule code — with
[`noqa-aliases`](suppression.md#aliasing-noqa-codes) covering old comments —
never forces a config rewrite.

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

konform uses `python` if set, otherwise auto-discovers `.venv`, `venv` or `.env`
in the project.

## Upgrading from an older config format

When the config shape changes (as in 0.3.0), konform auto-migrates an outdated
file in place the first time you run any command — comments and other
`[tool.*]` sections are preserved — and prints a summary to stderr.
