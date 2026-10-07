---
title: Suppressing violations
---

# Suppressing violations

```python
from os.path import join   # noqa: KIS001   ← exact rule
from os.path import join   # noqa: KIS       ← whole category
from os.path import join   # noqa             ← everything on this line
```

## Aliasing noqa codes

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

## Upgrading from an older config format

When konform's config shape changes (as it did in 0.3.0, moving rule
selection/settings under `[tool.konform.lint]`), it auto-migrates an
outdated `pyproject.toml` / `konform.toml` the first time you run any
konform command: the file is rewritten in place (comments and other
`[tool.*]` sections are preserved) and a summary of what changed is printed
to stderr. No action is needed beyond re-running konform once.
