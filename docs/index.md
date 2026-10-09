# konform

Multi-rule Python linter and language server — fast, configurable, and CI-ready.

!!! warning "Work in progress"
    konform is under active development. Rules, configuration keys, CLI flags,
    and the LSP surface may change at any time, including in
    backwards-incompatible ways, until a 1.0 release.

## Highlights

- **Module-only imports and alias policy** — rules [KIS001](rules/kis001.md)
  and [KIS002](rules/kis002.md) cover what [Ruff](https://github.com/astral-sh/ruff) doesn't (yet).
- **Your own rules** — define regex [pattern rules](rules/kpt.md) in TOML; no plugin code.
- **Auto-fixes** with a Ruff-style safe/unsafe split ([Fixes](fixes.md)).
- **Built-in language server** sharing the CLI's engine ([Editors](editors.md)).
- **CI-ready** output: GitHub, GitLab, SARIF, JUnit, JSON ([Output formats](reference/output-formats.md)).
- **Cached** per-file results ([Caching](caching.md)).

## Quick look

```console
$ pip install konform
$ konform check src/
```

## Where to go next

| I want to…                         | Read                                     |
| ---------------------------------- | ---------------------------------------- |
| Install and run konform            | [Installation](installation.md), [Tutorial](tutorial.md) |
| Configure it                       | [Configuration](configuration.md), [Settings](reference/settings.md) |
| Understand a rule                  | [Rules](rules/index.md)                  |
| Silence a violation                | [Suppressing violations](suppression.md) |
| Look up a flag                     | [CLI reference](reference/cli.md)        |
| Use it in my editor                | [Editors](editors.md)                    |

## Acknowledgements

konform was built to fill the gap left by Ruff, whose design inspired how konform
is configured, reports violations and resolves `src` roots. It is built on
[`ruff_python_parser`](https://crates.io/crates/ruff_python_parser),
[`ruff_python_ast`](https://crates.io/crates/ruff_python_ast) and
[`ruff_text_size`](https://crates.io/crates/ruff_text_size).
