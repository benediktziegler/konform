## v0.1.1 (2026-09-02)

### Fix

- **kis001**: detect src-layout and namespace subpackages as modules

## 0.1.0 (2026-09-02)

### Feat

- **config**: add aliases for noqa rule codes
- **kis001**: add unresolved import warning level config
- switch Python AST parser from rustpython-parser to ruff_python_parser
- add Zed editor extension
- add CLI, LSP server and main entry point
- add KPT user-defined pattern-matching rule
- add rule engine and KIS001 module-only import checker
- add config loader and git changed-file detection
- add core types, theme, module probe and file cache

### Fix

- update lsp-server Response field for 0.10.0
- normalize walked python file paths
- **module_probe**: handle cwd sys.path and cache race
- improve cache invalidation

### Refactor

- **main**: remove python wrapper and migrate to pure Rust binary
