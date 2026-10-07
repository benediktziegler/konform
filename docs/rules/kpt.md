---
title: KPT — User-defined pattern rules
---

# KPT — User-defined pattern rules

Load regex patterns from `konform_patterns.toml` (auto-discovered next to
`pyproject.toml`) or inline in `pyproject.toml`:

```toml
[[tool.konform.KPT.rules]]
id      = "KPT001"
message = "Use the project logger instead of bare print()."
pattern = '^\s*print\s*\('
files   = ["src/**/*.py"]
level   = "warning"
```

See [Configuration](../configuration.md#pattern-files) for pattern files and sub-rules.
