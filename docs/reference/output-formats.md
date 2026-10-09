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

## GitHub Actions

```yaml
- run: pip install konform
- run: konform check --output-format github .
```

## Zuul

`konform check --output-path PATH` writes a Zuul `zuul_return.yaml`
(default `tmp/zuul/zuul_return.yaml`).

## Exit codes

| Code | Meaning                                                                   |
| ---- | ------------------------------------------------------------------------- |
| 0    | No violations at or above `--level`, or `--exit-zero`/`--fix-only` given  |
| 1    | Violations found (`--silent` also exits 1 on violations)                  |
