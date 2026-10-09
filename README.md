# konform

[![CI](https://github.com/benediktziegler/konform/actions/workflows/ci.yml/badge.svg)](https://github.com/benediktziegler/konform/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/benediktziegler/konform/graph/badge.svg)](https://codecov.io/gh/benediktziegler/konform)
[![Docs](https://github.com/benediktziegler/konform/actions/workflows/docs.yml/badge.svg)](https://benediktziegler.github.io/konform/)
[![PyPI](https://img.shields.io/pypi/v/konform)](https://pypi.org/project/konform/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/benediktziegler/konform/blob/main/LICENSE)

Multi-rule Python linter and language server — fast, configurable, and CI-ready.

> **Work in progress.** konform is under active development. Rules,
> configuration keys, CLI flags, and the LSP surface may change at any time,
> including in backwards-incompatible ways, until a 1.0 release.

**[Documentation](https://benediktziegler.github.io/konform/)**

## Why konform?

[Ruff](https://github.com/astral-sh/ruff) covers most of what a Python project
wants from a linter, but not everything. konform is a small, fast Rust linter
for the checks that fall through the gaps. Its rules are **opinionated**: they
encode a specific style (such as the
[Google Python Style Guide](https://google.github.io/styleguide/pyguide.html)'s
module-only imports) rather than universal correctness, and the list keeps
growing.

- **Opinionated import rules** — enforce module-only imports (KIS001) and flag
  needless `from X import Y as Z` aliases (KIS002).
- **Project-specific rules** — define your own regex rules (KPT) in TOML, with
  messages, file globs and optional replacements. No plugin code.
- **Auto-fixes** with a Ruff-style safe/unsafe split.
- **Built-in language server** that shares the CLI's engine.
- **CI-ready** output: GitHub, GitLab, SARIF, JUnit, Zuul and JSON.

## Installation

```bash
pip install konform
```

Wheels ship a pre-compiled Rust binary — no Rust installation needed at runtime.

## Example

```python
# app.py
from os.path import join

print(join("a", "b"))
```

```console
$ konform check app.py
error[KIS001][*]: Import 'join' from 'os.path' is not a module.
  --> app.py:1:1
  help: Use only module imports, see: https://google.github.io/styleguide/pyguide.html#22-imports

Found 1 error.
[*] 1 fixable with the `--fix` option.

$ konform check --fix app.py
```

Other useful commands:

```bash
konform check --fix --unsafe-fixes src/   # also apply unsafe fixes (e.g. KIS002)
konform check --output-format github .    # annotations for GitHub Actions
konform rule --list                       # list rules
konform rule --explain KIS001             # explain a rule
konform server                            # start the language server
```

Configure it in `pyproject.toml` (or `konform.toml`):

```toml
[tool.konform.lint]
select = ["KIS"]

[tool.konform.lint.module-only-imports]
exceptions = ["typing", "mycompany.compat"]
```

See the [documentation](https://benediktziegler.github.io/konform/) for the full
[rule](https://benediktziegler.github.io/konform/rules/),
[configuration](https://benediktziegler.github.io/konform/reference/settings/),
[CLI](https://benediktziegler.github.io/konform/reference/cli/) and
[editor](https://benediktziegler.github.io/konform/editors/) reference.

## Development

You need a Rust toolchain (pinned in `rust-toolchain.toml`) and [uv](https://docs.astral.sh/uv/).

```bash
# Build and test
cargo build
cargo test
cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings

# Run the dev binary
cargo run -- check path/to/file.py

# Build the Python package (wheel with the compiled binary)
uv run maturin build --release        # wheels land in target/wheels/
uv run maturin develop                # or install into the project venv

# Regenerate the rules docs from the rule definitions (docs/rules/, checked by a test)
cargo run -- rule --generate-docs docs/rules

# Build and preview the documentation
uv run --only-group dev --python 3.12 zensical serve
uv run --only-group dev --python 3.12 zensical build --clean --strict
```

The docs source lives in `docs/` (config in `mkdocs.yml`; the landing page is this README) and is deployed to
GitHub Pages by `.github/workflows/docs.yml` on every push to `main`.
Commits follow [Conventional Commits](https://www.conventionalcommits.org/);
releases are cut with [commitizen](https://commitizen-tools.github.io/commitizen/).

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

## License

[MIT](https://github.com/benediktziegler/konform/blob/main/LICENSE)
