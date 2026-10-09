## Unreleased

### Feat

- **rules**: reject weak KNQ001 reasons
- **rules**: add placeholder reasons and exempt codes to KNQ001
- **rules**: add --reason and KNQ002 noqa style rule
- **rules**: make KNQ001 opt-in via extend-select
- **rules**: add KNQ001 requiring a reason on noqa
- **kst**: add rule --schema for editor completion
- **kst**: add rule --test with embedded snippets
- **kst**: add konform ast to print the rule-visible tree
- **kst**: fail hard on invalid rules in the CLI
- **kst**: resolve simple alias assignments
- **kst**: add user-defined structural rule engine
- **rule**: render `rule --explain` as formatted terminal output
- **rules**: generate the rule docs from structured Rule::docs()
- **output**: make Zuul a --output-format instead of a default side effect
- **init**: insert the config after ruff blocks and test the init flows
- **kis002**: allow multi-module aliases and add alias-template

### Fix

- **docs**: list each environment variable once
- **rules**: warn on invalid KNQ settings
- **output**: make the CI writers schema-correct and escape user data
- **output**: point SARIF URLs at the real repository
- **cache**: key cache on rule config and pattern files
- **rule**: list and explain user-defined patterns
- **kpt**: honor select/ignore per pattern id
- **noqa**: ignore empty codes and non-comment matches

### Refactor

- **cli**: rename --reason to --noqa-reason
- **kst**: isolate parser types behind a node layer
- drop unsafe impls, dead code and unused dependencies

### Perf

- **rules**: parse each file once
- **lsp**: build rules once per config load
- **kpt**: compile patterns once per run

## v0.4.0 (2026-09-29)

### Feat

- add --unsafe-fixes flag and split fix safety

### Fix

- place --unsafe-fixes before paths in fix hint
- **kis002**: skip error for aliases exported in __all__
- warn when python module probe is unusable

## v0.3.0 (2026-09-23)

### Feat

- **rules**: add KIS002 unnecessary import alias rule
- **config**: add lint config migration framework
- **zed**: auto-install konform binary from releases

### Fix

- **lsp**: add targeted quickfixes and honor context.only
- **kis001**: skip autofix for overlapping imports

## v0.2.0 (2026-09-09)

### Feat

- **config**: add configurable module probe src roots

### Fix

- **kis001**: track nested global names as module scope
- resolve module probe cwd from lint target root
- **kis001**: handle match-case pattern scope bindings
- **kis001**: preserve import scope and use scope-aware shadow checks

### Refactor

- **kis001**: simplify nested insert loop indexing
- **kis001**: extract scope and import fix helpers

## v0.1.2 (2026-09-04)

### Fix

- **module_probe**: detect builtin stdlib modules
- **engine**: iterate fixes to stable valid output

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
