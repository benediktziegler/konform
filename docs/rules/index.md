---
title: Rules
---

Every konform rule has a code, a stable config name (shown by `konform rule --list`),
and its own table under `[tool.konform.lint]`. Use `konform rule --explain <CODE>` for the
same documentation in your terminal.

| Code | Name | Fixable | Page |
| ---- | ---- | ------- | ---- |
| KIS001 | `module-only-imports` | sometimes | [Module-only imports](kis001.md) |
| KIS002 | `import-alias-policy` | sometimes (unsafe) | [Import alias policy](kis002.md) |
| KPT001 | `user-defined-patterns` | no | [User-defined pattern rules](kpt.md) |
