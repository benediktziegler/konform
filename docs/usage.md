---
title: Usage
---

# Usage

```bash
# Lint all Python files under src/
konform check src/

# Lint and apply auto-fixes in one pass
konform check --fix src/

# Also apply fixes marked unsafe (e.g. KIS002)
konform check --fix --unsafe-fixes src/

# Apply fixes only (no lint report)
konform check --fix src/

# Show a unified diff of what format would change
konform check --diff src/

# Output violations as JSON (e.g. for tooling)
konform check --output-format json src/

# Suppress hints and summary (violations only)
konform check -q src/

# No output — just exit 1 on violations
konform check -s src/

# List all rules
konform rule --list

# Explain a rule
konform rule --explain KIS001

# Clear the local cache
konform clean
```

When some reported violations are fixable only via `--unsafe-fixes` (e.g.
KIS002), the summary breaks safe and unsafe-only counts out separately, and
the suggested fix command is adjusted accordingly.

## CLI reference

```text
konform check  [OPTIONS] <PATHS>…    Lint files (default subcommand)
konform check --fix-only [OPTIONS] <PATHS>…  Apply all auto-fixes in-place, exit 0
konform server                       Start the LSP server (stdin/stdout)
konform rule   --list                List all rules
konform rule   --explain <CODE>      Show full rule documentation
konform clean  [--config PATH]       Delete the cache directory
konform version                      Print konform's version

Global options (available on all subcommands):
  --color auto|always|never          Colour output control
  --isolated                         Ignore all config files
  -v / --verbose                     Extra output
  -q / --quiet                       Violations only (no summary/hints)
  -s / --silent                      No output; exit code only
```
