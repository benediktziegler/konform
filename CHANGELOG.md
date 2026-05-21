# Changelog

<!-- changelog managed by commitizen (cz bump) -->

## [0.1.0] — Initial release

### Features

- Rust-based import checker (IS001: module-only imports)
- Filesystem-based module probe — no Python execution at check time
- SHA-256 file-level result cache (`.konform_cache/`)
- Auto-fix mode (`--fix`)
- Git-aware changed-file filtering
- Zuul `zuul_return.yaml` output for CI integration
- Configurable via `[tool.konform]` in `pyproject.toml` or `konform.toml`
- Cross-compiled wheels for Linux x86_64/aarch64, Windows x86_64, macOS x86_64/aarch64
- Python wrapper for zero-overhead `pip install` distribution
