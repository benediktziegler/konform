# Output formats

Select with `--output-format`; redirect with `-o/--output-file`.

| Format    | Destination | Description                                                         |
| --------- | ----------- | ------------------------------------------------------------------- |
| `full`    | stderr      | Ruff-style output with arrows and help lines (default)              |
| `concise` | stderr      | One line per violation: `file:line:col: level[RULE] message`; `[*]` marks fixable |
| `json`    | stdout      | JSON array, machine-readable                                        |
| `github`  | stdout      | GitHub Actions workflow-command annotations                         |
| `gitlab`  | stdout      | GitLab Code Quality JSON report                                     |
| `sarif`   | stdout      | SARIF 2.1.0 JSON                                                    |
| `junit`   | stdout      | JUnit XML report                                                    |
| `zuul`    | file        | Zuul `zuul_return.yaml`, written to `--output-path`                 |

## GitHub Actions

```yaml
- run: pip install konform
- run: konform check --output-format github .
```

## Zuul

`--output-format zuul` writes a Zuul `zuul_return.yaml` to `--output-path` (default
`tmp/zuul/zuul_return.yaml`, directories are created) and prints nothing else. Nothing is
written for the other formats.

Existing content of the file is preserved; only `data.zuul.file_comments` and
`data.zuul.warnings` are replaced. Violations in files changed in the current git
change (staged/unstaged changes, or the last commit) become file comments; all others
become warnings. The `help` text of a violation is appended to its message.

```console
$ konform check --output-format zuul --output-path zuul_return.yaml .
```

With `--output-file`, the violations are additionally written to that file as JSON.

## Exit codes

| Code | Meaning                                                                   |
| ---- | ------------------------------------------------------------------------- |
| 0    | No violations at or above `--level`, or `--exit-zero`/`--fix-only` given  |
| 1    | Violations found (`--silent` also exits 1 on violations)                  |
