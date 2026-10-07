---
title: Development
---

# Development

```bash
# Compile the Rust binary and install it in the dev venv (required before tests)
hatch run develop

# Run tests with coverage
hatch test -c

# Build release wheels for all platforms
hatch run maturin:build-all
```

# Acknowledgements

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
