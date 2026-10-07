---
title: Configuration
---

# Configuration

Add a `[tool.konform]` section to `pyproject.toml` (or a standalone `konform.toml`):

```toml
[tool.konform]
cache-dir = ".konform_cache"
workers   = 0         # 0 = os.cpu_count()
src       = [".", "src"]   # search roots for KIS001's module-existence probe;
                            # see "Module search roots" below.

[tool.konform.lint]
select = []        # [] = all rules; prefix match: "KIS" = all KIS* rules
ignore = []
level  = "error"   # "warning" | "error"

# ── KIS001 — import style ──────────────────────────────────────────────────
[tool.konform.lint.module-only-imports]
exceptions = [
    "__future__", "typing", "typing_extensions", "collections.abc",
    "mycompany.compat",
]
level = "error"
unresolved-level = "warning"   # "warning" (default) | "error" | "off"
                                # Used when a package isn't installed in this
                                # environment, so KIS001 can't tell whether the
                                # imported name is a module or not.

# ── KIS002 — import alias policy ──────────────────────────────────────────
[tool.konform.lint.import-alias-policy]
level = "warning"
# Aliased imports of the same name from different modules in one scope are
# never flagged (the aliases keep them apart).
# Optional: also always allow aliases that match this template, even when the
# rename isn't needed (it only exempts aliases, never flags them).
# Placeholders: {name}, {module}, {module_first}, {module_last}; no other
# braces. An invalid template is reported on stderr and ignored.
# alias-template = "{module_last}_{name}"   # foo.bar -> baz as bar_baz

# ── KPT001 — user-defined patterns ─────────────────────────────────────────
[tool.konform.lint.user-defined-patterns]
level = "warning"
# Optional: load patterns from an external file instead of inline rules.
# rules_file = "konform_patterns.toml"

[[tool.konform.lint.user-defined-patterns.rules]]
id      = "KPT001"
message = "Use the project logger instead of bare print()."
pattern = '^\s*print\s*\('
files   = ["src/**/*.py"]
level   = "warning"

# Sub-rules are attached to this rule entry. This inline form makes the
# parent/child relation explicit and avoids table-order confusion.
sub_rules = [
  {
    pattern = ['print\(.*password', 'print\(.*secret'],
    message = "Never print credentials.",
    help = "Use redaction helpers before logging.",
  },
]
```

Each rule's settings live in its own table, keyed by a stable config name
(shown by `konform rule --list`) rather than by rule code — so renaming a
rule code (with `noqa-aliases` covering old suppression comments) never
forces you to also rewrite your config.

When using `[[tool.konform.lint.user-defined-patterns.rules.sub_rules]]`, TOML
binds each sub-rule to _the most recently declared_
`[[tool.konform.lint.user-defined-patterns.rules]]` entry.

## Pattern files

Patterns can also live in a standalone `konform_patterns.toml` placed next to
`pyproject.toml`. konform auto-discovers it (no config key needed):

```toml
# konform_patterns.toml
[[rules]]
id      = "KPT002"
message = "Remove breakpoint() — debugging artefact."
pattern = '^\s*breakpoint\s*\(\s*\)'
level   = "error"
```

## Module search roots

KIS001 needs to know whether an imported name is a real module (`import os.path`)
or just an attribute of one (`from os.path import join`). It answers this by
searching the filesystem, starting from your Python environment's `sys.path`
plus a configurable set of extra roots -- this matters for local packages that
aren't installed (e.g. a `src/` layout, or code laid out some other way).

The extra roots are resolved the same way as Ruff's `src` setting, including
its precedence:

1. `[tool.konform] src = [...]`, if set.
2. Otherwise, `[tool.ruff] src = [...]`, if your project already configures
   Ruff for a non-standard layout.
3. Otherwise, the default `[".", "src"]` (covers both flat and `src` layouts
   out of the box).

Each entry is resolved relative to the directory containing `pyproject.toml`
/ `konform.toml`. For example, if your package lives under `lib/`:

```toml
[tool.konform]
src = ["lib"]
```
