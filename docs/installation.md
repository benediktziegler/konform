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

## Verifying release artifacts

Every wheel, sdist and standalone binary attached to a [GitHub release](https://github.com/benediktziegler/konform/releases) carries a signed [build provenance attestation](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations), proving it was built by konform's release workflow from the tagged commit. Check a downloaded file with the [GitHub CLI](https://cli.github.com/):

```console
$ gh attestation verify konform-x86_64-unknown-linux-gnu --repo benediktziegler/konform
```

Add `--signer-workflow benediktziegler/konform/.github/workflows/release.yml` to also pin the workflow that produced it. Packages on PyPI additionally carry PyPI's own [publish attestations](https://docs.pypi.org/attestations/).

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
