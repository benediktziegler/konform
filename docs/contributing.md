# Contributing

You need a Rust toolchain (pinned in `rust-toolchain.toml`) and [uv](https://docs.astral.sh/uv/).

## Build and test

```console
$ cargo build
$ cargo test
$ cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings
$ cargo run -- check path/to/file.py     # run the dev binary
```

## Build the Python package

```console
$ uv run maturin build --release         # wheels land in target/wheels/
$ uv run maturin develop                 # install into the project venv
```

## Build the docs

The pages under `docs/rules/` are generated from the rule definitions in `src/rules/`
(`Rule::docs()`, plus fix safety and config names read from the `Rule` trait). Edit the
rule, then regenerate; a test fails if the committed pages are stale.

```console
$ cargo run -- rule --generate-docs docs/rules
$ uv run --only-group dev --python 3.12 zensical serve
$ uv run --only-group dev --python 3.12 zensical build --clean --strict
```

`serve` previews at <http://127.0.0.1:8000>; the `build` line is what CI runs. The
site is deployed to GitHub Pages by `.github/workflows/docs.yml` on every push to
`main` that touches the docs.

## Commits and releases

Commits follow [Conventional Commits](https://www.conventionalcommits.org/);
releases are cut with [commitizen](https://commitizen-tools.github.io/commitizen/).
