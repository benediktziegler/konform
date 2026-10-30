# Contributing

You need a Rust toolchain (pinned in `rust-toolchain.toml`) and [uv](https://docs.astral.sh/uv/).

## Build and test

```console
$ cargo build
$ cargo test
$ cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings
$ cargo run -- check path/to/file.py     # run the dev binary
```

## Performance

Two checks in `.github/workflows/benchmarks.yml` guard against slowdowns, the same
split ruff and ty use:

- **Benchmarks** (`benches/check.rs`): the engine on generated modules of three sizes,
  each rule family on its own, and `--fix`. CI runs them under
  [CodSpeed](https://codspeed.io), which counts instructions rather than timing them, so
  results are stable on shared runners; CodSpeed comments on the PR when a benchmark gets
  slower. A new rule is covered as soon as its family runs, but give the generated module
  in `python_module` something for it to report.
- **Ecosystem check** (`scripts/ecosystem_check.py`): the PR's binary against its base on
  pinned real projects (django, sympy, flask), cold and warm. It fails when a cold run is
  more than 15% slower, and diffs the diagnostics, so a behaviour change shows in the same
  report (the job summary).

```console
$ cargo bench --bench check                      # wall-clock, local
$ cargo bench --bench check -- check_rule_family # one group
$ cargo build --profile dev-release && cp target/dev-release/konform /tmp/konform-new
$ git switch main && cargo build --profile dev-release
$ scripts/ecosystem_check.py target/dev-release/konform /tmp/konform-new
```

## Build the Python package

```console
$ uv run maturin build --release         # wheels land in target/wheels/
$ uv run maturin develop                 # install into the project venv
```

## Build the docs

Three parts of the reference are generated from the code and must not be edited by hand:

- `docs/rules/`: from the rule definitions in `src/rules/` (`Rule::docs()`, plus fix safety
  and config names read from the `Rule` trait).
- `docs/reference/cli.md` and `docs/reference/environment-variables.md`: from the clap
  definitions in `src/cli.rs` (the doc comments on the arguments, their defaults, value
  names, possible values and `env` bindings).

Change the code, then regenerate with `cargo run -- generate-docs docs`. The
`generate-docs` pre-commit hook does this on every commit that touches the code or the
generated pages (the commit stops so you can stage the updated pages), and a test fails if
the committed pages are stale.

```console
$ cargo run -- generate-docs docs
$ uv run --only-group dev --python 3.12 zensical serve
$ uv run --only-group dev --python 3.12 zensical build --clean --strict
```

`serve` previews at <http://127.0.0.1:8000>; the `build` line is what CI runs. The
site is deployed to GitHub Pages by `.github/workflows/docs.yml` on every push to
`main` that touches the docs.

## Commits and releases

Commits follow [Conventional Commits](https://www.conventionalcommits.org/);
releases are cut with [commitizen](https://commitizen-tools.github.io/commitizen/).
