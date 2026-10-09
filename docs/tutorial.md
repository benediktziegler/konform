# Tutorial

## Set up a project

```console
$ konform init              # in the project root
```

If the directory has a `pyproject.toml`, `init` adds a `[tool.konform]` section to it
(after the last `[tool.ruff…]` block if you have one, otherwise at the end, with blank lines around it); otherwise it creates a `konform.toml`. It also creates a starter
`konform_patterns.toml` for [custom pattern rules](rules/kpt.md). Nothing is changed if
`[tool.konform]` is already configured.

```console
$ konform init --diff       # preview what would be created, write nothing
$ konform init --no-patterns
$ konform init --force      # create konform.toml even if pyproject.toml exists
```

See [Configuration](configuration.md) for what to put in the generated file.

## Lint a project

```console
$ konform check src/          # lint everything under src/
$ konform check .             # lint the current directory
```

`check` is the default subcommand, so `konform src/` works too.

## Fix violations

```console
$ konform check --fix src/                    # safe fixes
$ konform check --fix --unsafe-fixes src/     # also unsafe fixes (e.g. KIS002)
$ konform check --diff src/                   # preview as a unified diff
```

See [Fixes](fixes.md) for the safe/unsafe distinction.

## Choose rules

```console
$ konform check --select KIS src/             # only KIS* rules
$ konform check --extend-select KPT src/      # add KPT* rules on top of config
$ konform check --ignore KIS002 src/
```

Codes are prefix-matched: `KIS` selects every `KIS*` rule.

## Explore rules

```console
$ konform rule --list
$ konform rule --explain KIS001
```

## Machine-readable output

```console
$ konform check --output-format json src/
$ konform check --statistics src/
```

## Verbosity

```console
$ konform check -q src/    # violations only
$ konform check -s src/    # no output, exit code only
```

## Next steps

- [Configure](configuration.md) konform in `pyproject.toml`.
- Add [custom pattern rules](rules/kpt.md).
- Run it in your [editor](editors.md).
