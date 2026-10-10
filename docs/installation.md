# Installation

=== "uv"

    ```console
    $ uv tool install konform     # isolated, on your PATH
    $ uvx konform check src/      # or run it without installing
    ```

=== "pipx"

    ```console
    $ pipx install konform
    ```

=== "pip"

    ```console
    $ pip install konform
    ```

Wheels ship a pre-compiled Rust binary, so no Rust installation is needed at runtime.

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

## Shell completions

`konform completions <shell>` prints a completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`:

```console
$ konform completions zsh > ~/.zfunc/_konform                      # zsh (~/.zfunc must be in $fpath)
$ konform completions bash > ~/.local/share/bash-completion/completions/konform
$ konform completions fish > ~/.config/fish/completions/konform.fish
```

## pre-commit

Add konform to your `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/benediktziegler/konform
    rev: v0.4.0  # pin a release tag
    hooks:
      - id: konform        # lint only
      # - id: konform-fix  # lint and apply safe fixes; fails the commit when it changed files
```

pre-commit builds konform from source with cargo (installing a Rust toolchain if needed), so the first run takes a minute; later runs use the cached build.
