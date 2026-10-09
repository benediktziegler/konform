# Fixes

Some rules can fix their violations automatically.

| Flag                | Behaviour                                                         |
| ------------------- | ----------------------------------------------------------------- |
| `--fix`             | Apply safe fixes, then report what remains                        |
| `--fix --unsafe-fixes` | Also apply fixes marked *unsafe*                               |
| `--fix-only`        | Apply fixes, don't report remaining violations; exit 0            |
| `--diff`            | Print a unified diff instead of writing; exit 1 if anything would change |
| `--exit-non-zero-on-fix` | Exit non-zero if `--fix` modified files                      |

## Safe vs. unsafe

A fix is **unsafe** when konform cannot rule out changing program behaviour, for
example other dynamic references to a renamed alias. [KIS002](rules/kis002.md)'s
fix is unsafe and needs `--unsafe-fixes`.

When some violations are fixable only with `--unsafe-fixes`, the summary reports
safe and unsafe-only counts separately and suggests the right command.

Fixes are skipped (the violation is still reported) when applying one would cause
a name collision; see each rule page for the conditions.

## Editors

The language server offers per-violation quickfixes plus *Fix all* code actions;
see [Editors](editors.md).
