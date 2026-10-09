# Installation

```console
$ pip install konform
```

Wheels ship a pre-compiled Rust binary — no Rust installation is needed at runtime.

Verify the install:

```console
$ konform version
```

## Initialising a project

`konform init` adds a `[tool.konform]` section to an existing `pyproject.toml` (after any Ruff blocks) (or creates `konform.toml` if there is none) and adds a `konform_patterns.toml`:

```console
$ konform init
```

| Flag            | Effect                                                                  |
| --------------- | ----------------------------------------------------------------------- |
| `--force`       | Create `konform.toml` even if `pyproject.toml`/`konform.toml` exists    |
| `--no-patterns` | Skip creating `konform_patterns.toml`                                   |
| `--diff`        | Show what would be created without writing files                        |
