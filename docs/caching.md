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

## Cache format and old caches

Cache files are stored with [rkyv](https://rkyv.org) and begin with a short header
(magic bytes plus a format version). Every file is validated before use. A file
that is corrupt, larger than 256 MiB, or in any other format (for example the
bincode format used by earlier development builds) is silently ignored and
replaced by the next run, so no manual migration is needed.

Each konform version keeps its cache in its own subdirectory of `cache-dir`.
On startup, konform removes subdirectories of other released versions that
have not been modified for 30 days. `konform clean` removes the whole cache
directory, including all versions.

## What `konform clean` removes

`konform clean` deletes the directory named by `cache-dir`, resolved against
the repository root found from the current directory (the nearest ancestor
holding `.git` or `pyproject.toml`). If that directory does not exist, it also
looks for `<cwd>/<cache-dir>`, which covers caches that older versions left in
a subdirectory.

It removes **any** directory that carries a `CACHEDIR.TAG` file, including an
absolute `cache-dir` or one that points outside the project such as
`../sibling`. Point `cache-dir` only at directories you are happy to lose.

`clean` refuses (exit code 1) and deletes nothing when:

- the directory has no `CACHEDIR.TAG`, so it does not look like a konform cache;
- it is the repository root, the current directory, or an ancestor of either
  (checked on fully resolved paths, so symlinks and `..` cannot hide this);
- the cache directory, the repository root or the current directory cannot be
  resolved to a canonical path.

If `cache-dir` is a symlink, only the link is removed, not its target.
