# Command-line interface

```text
konform [GLOBAL OPTIONS] <COMMAND>
```

| Command                  | Description                                          |
| ------------------------ | ---------------------------------------------------- |
| [`check`](#check)        | Lint files (default when no subcommand is given)     |
| [`rule`](#rule)          | List or explain rules                                |
| [`init`](#init)          | Create a starter configuration                       |
| [`clean`](#clean)        | Delete the cache                                     |
| [`server`](#server)      | Start the language server over stdin/stdout          |
| `version`                | Print konform's version                              |

## Global options

| Option          | Description                                                              |
| --------------- | ------------------------------------------------------------------------ |
| `--color <WHEN>`| `auto` (default), `always`, `never`. Env: `KONFORM_COLOR`                |
| `--isolated`    | Ignore all configuration files; use built-in defaults                    |
| `-v, --verbose` | Enable verbose logging                                                   |
| `-q, --quiet`   | Print violations only; suppress hints, summary and progress              |
| `-s, --silent`  | Suppress all output; exits 1 on violations, 0 otherwise                  |

## `check`

```text
konform check [OPTIONS] <FILE_PATHS>...
```

`FILE_PATHS` are files or directories; use `.` for the current directory and `-` for stdin.

### Rule selection

| Option                      | Description                                                         |
| --------------------------- | ------------------------------------------------------------------- |
| `--select <CODES>`          | Enable only these codes/prefixes (comma-separated); overrides config |
| `--ignore <CODES>`          | Disable these codes/prefixes; merged with config                    |
| `--extend-select <CODES>`   | Add codes on top of the configured selection                        |
| `--extend-ignore <CODES>`   | Add ignores on top of the configured list                           |

### File selection

| Option                             | Description                                                |
| ---------------------------------- | ---------------------------------------------------------- |
| `--exclude <GLOBS>`                | Exclude files matching these globs (comma-separated)       |
| `--extend-exclude <GLOBS>`         | Add exclusions on top of the configured ones               |
| `--per-file-ignores <GLOB:CODES>`  | e.g. `"tests/**:KIS001,KPT"`; replaces the configured table |
| `--extend-per-file-ignores <…>`    | Merge with the configured table                            |
| `--stdin-filename <NAME>`          | Filename to display/match when reading stdin (`-`)         |
| `--show-files`                     | Print the files konform would check, then exit 0           |
| `--config <PATH>`                  | Use a specific `pyproject.toml` / `konform.toml`           |

### Fixing

| Option                   | Description                                                      |
| ------------------------ | ---------------------------------------------------------------- |
| `--fix`                  | Apply safe fixes in place, then report the remainder             |
| `--unsafe-fixes`         | Also apply unsafe fixes                                          |
| `--fix-only`             | Apply fixes, don't report remaining violations (implies `--fix`) |
| `--diff`                 | Print a unified diff of what `--fix` would change; exit 1 if any |
| `--exit-non-zero-on-fix` | Exit non-zero if `--fix` modified files                          |
| `--add-noqa`             | Append `# noqa: CODE` to violating lines, then exit 0            |
| `--ignore-noqa`          | Ignore `# noqa` comments                                         |

### Output

| Option                     | Description                                                   |
| -------------------------- | ------------------------------------------------------------- |
| `--output-format <FMT>`    | See [Output formats](output-formats.md). Default `full`       |
| `-o, --output-file <PATH>` | Write output to a file                                        |
| `--statistics`             | Show violation counts per rule code                           |
| `--output-path <PATH>`     | Zuul `zuul_return.yaml` path (default `tmp/zuul/zuul_return.yaml`) |

### Miscellaneous

| Option                         | Description                                              |
| ------------------------------ | -------------------------------------------------------- |
| `--level <LEVEL>`              | Minimum severity causing a non-zero exit (default `error`) |
| `--changed-files-level <LEVEL>`| Override exit severity for changed-file violations       |
| `-e, --exit-zero`              | Always exit 0                                            |
| `-n, --no-cache`               | Bypass the result cache                                  |
| `--cache-dir <PATH>`           | Override the cache directory                             |
| `-w, --watch`                  | Re-run on `.py` changes (Ctrl-C to stop)                 |

## `rule`

| Option             | Description                          |
| ------------------ | ------------------------------------ |
| `--list`           | List all rules, including project KPT patterns |
| `--explain <CODE>` | Print full documentation for a rule  |

## `init`

```text
konform init [OPTIONS] [PATH]
```

See [Installation](../installation.md#initialising-a-project).

## `clean`

```text
konform clean [--config <PATH>]
```

Deletes the cache directory found via the configuration.

## `server`

```text
konform server
```

See [Editors](../editors.md).
