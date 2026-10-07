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
[KNQ001](rules/knq001.md) can require that explanation, and [KNQ002](rules/knq002.md)
wants it in its own `# ...` comment.

## Adding `noqa` automatically

```console
$ konform check --add-noqa src/
```

Appends `# noqa: CODE` to each violating line. Codes merge into an existing list
(sorted, deduplicated); a bare `# noqa` is left untouched.

### Baselining with `--add-noqa`

`konform check --add-noqa` appends `# noqa: CODE` to every line with a violation (merging
into an existing `# noqa: ...`; a blanket `# noqa` is left as is). Pass `--reason` to
record why, once, for the whole baseline:

```console
$ konform check --add-noqa --reason "legacy, tracked in ABC-123" src/
# from os.path import join  # noqa: KIS001  # legacy, tracked in ABC-123
```

With `--extend-select KNQ001`, the reason also fills in existing `# noqa` comments that lack
one. A reason that is already there is never overwritten. The flag requires `--add-noqa`.

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
