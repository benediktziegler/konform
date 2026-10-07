---
title: konform
description: Multi-rule Python linter and language server
---

# konform

Multi-rule Python linter and language server — fast, configurable, and CI-ready.

> **Work in progress.** konform is under active development. Rules,
> configuration keys, CLI flags, and the LSP surface may change at any time,
> including in backwards-incompatible ways, until a 1.0 release.

## Highlights

- **Fast** — a single pre-compiled Rust binary, parallel across files, with a local cache.
- **Configurable** — Ruff-style `[tool.konform]` configuration in `pyproject.toml`.
- **Fixable** — safe and unsafe auto-fixes, with `--diff` previews.
- **Editor-ready** — a built-in [language server](editors.md) sharing the CLI's rule engine.
- **Extensible** — [user-defined regex rules](rules/kpt.md) for project conventions.

## Quick start

```bash
pip install konform
konform check src/
```

See [Installation](installation.md), [Usage](usage.md) and the [rule reference](rules/index.md).
