# Caching

Results are cached per file, keyed by mtime and permissions, in `cache-dir`
(default `.konform_cache`). Entries not used (hit or re-linted) for 30 days are evicted.

The cache is also keyed by every setting that affects results: `select`,
`extend-select`, `ignore`, the Python environment, rule config tables, `per-file-ignores`,
`noqa-aliases`, and the content of user-defined patterns and structural rules
(including `konform_patterns.toml` and `konform_rules.toml`). Editing any of them re-lints unchanged files.

| Action                  | How                                   |
| ----------------------- | ------------------------------------- |
| Bypass for one run      | `konform check --no-cache`            |
| Use another directory   | `konform check --cache-dir PATH`      |
| Delete the cache        | `konform clean [--config PATH]`       |

Concurrent runs sharing a cache directory never corrupt it (each write goes to a
temporary file and is renamed into place), but the last writer wins: entries
added by another run in the meantime may be lost and are simply re-linted next time.
