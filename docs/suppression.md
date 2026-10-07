# Suppressing violations

## `# noqa` comments

```python
from os.path import join   # noqa: KIS001   exact rule
from os.path import join   # noqa: KIS       whole category
from os.path import join   # noqa           everything on this line
```

- Multiple codes are comma-separated: `# noqa: KIS001, KPT010`.
- Empty entries are ignored; `# noqa:` with no codes behaves like a bare `# noqa`.
- Only real comments count — `# noqa` inside a string literal suppresses nothing.
- `--ignore-noqa` reports every violation regardless.
Text after the codes is a free-form explanation and never changes what is suppressed:
`# noqa: KIS001  # re-exported for plugins` suppresses `KIS001` only.
[KNQ001](rules/knq001.md) can require that explanation.

## Adding `noqa` automatically

```console
$ konform check --add-noqa src/
```

Appends `# noqa: CODE` to each violating line. Codes merge into an existing list
(sorted, deduplicated); a bare `# noqa` is left untouched.

## Config-level suppression

```toml
[tool.konform.lint]
ignore = ["KIS002"]

[tool.konform.lint.per-file-ignores]
"tests/**" = ["KIS001", "KPT"]
```

## Aliasing noqa codes

When a rule code changes, or you migrate from another linter's codes, aliases
keep old comments working:

```toml
[tool.konform.lint.noqa-aliases]
IS001 = "KIS001"
IS    = "KIS"
```

```python
from os.path import join   # noqa: IS001   suppresses KIS001 via the alias
```
