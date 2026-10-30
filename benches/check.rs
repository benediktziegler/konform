//! Benchmarks for the lint engine, the hot path of `konform check` and the
//! language server.
//!
//! Run locally with `cargo bench --bench check`. In CI they run under
//! CodSpeed (see `.github/workflows/benchmarks.yml`), which measures
//! instruction counts instead of wall time, so a PR's result is comparable
//! with `main` even on a noisy shared runner.
//!
//! The inputs are generated (see [`python_module`]) rather than vendored, so
//! they are deterministic and license-free, and every rule family has
//! something to report on every size.

use divan::{black_box, Bencher};
use konform::config::{load_config, resolve_python, Config};
use konform::engine::{run_check, run_fix, CheckInput};
use konform::module_probe::ModuleProbe;
use konform::rules::{all_rules, Rule};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

fn main() {
    divan::main();
}

/// File sizes, in repetitions of the module body in [`python_module`]
/// (about 70 lines each): a small module, a typical one, and a huge one.
const SIZES: &[usize] = &[1, 20, 100];

/// Rule families, benchmarked one at a time so a regression points at the
/// family that caused it.
const FAMILIES: &[&str] = &["KIS", "KNQ", "KPT", "KST"];

/// The benchmark project: config, KPT patterns and KST rules from
/// `benches/fixtures`, and a module probe for the runner's Python, built once
/// so starting the interpreter is not part of any measurement.
struct Project {
    config: Config,
    rules: Vec<Box<dyn Rule>>,
}

static PROJECT: LazyLock<Project> = LazyLock::new(|| {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures");
    let config = load_config(Some(&dir), None);
    assert!(config.errors.is_empty(), "{:?}", config.errors);
    let probe = ModuleProbe::new(&resolve_python(&config), &dir, &config.src);
    let rules = all_rules(Arc::new(probe), config.config_dir.clone());
    Project { config, rules }
});

fn path() -> PathBuf {
    PathBuf::from("src/pkg/module.py")
}

#[divan::bench(args = SIZES)]
fn check_file(bencher: Bencher, size: usize) {
    let project = &*PROJECT;
    let source = python_module(size);
    let path = path();
    bencher.bench(|| {
        let input = CheckInput::new(&path, black_box(&source));
        black_box(run_check(&input, &project.rules, &project.config))
    });
}

#[divan::bench(args = FAMILIES)]
fn check_rule_family(bencher: Bencher, family: &str) {
    let project = &*PROJECT;
    let mut config = project.config.clone();
    config.lint.select = vec![family.to_owned()];
    let source = python_module(20);
    let path = path();
    bencher.bench(|| {
        let input = CheckInput::new(&path, black_box(&source));
        black_box(run_check(&input, &project.rules, &config))
    });
}

/// `--fix` re-checks after every applied fix, so the huge size would
/// dominate the CI run without telling us more than the typical one.
#[divan::bench(args = &SIZES[..2])]
fn fix_file(bencher: Bencher, size: usize) {
    let project = &*PROJECT;
    let source = python_module(size);
    let path = path();
    bencher.bench(|| {
        let input = CheckInput::new(&path, black_box(&source));
        black_box(run_fix(&input, &project.rules, &project.config, true).unwrap())
    });
}

/// A Python module of `repeat` copies of a body that gives every rule family
/// work: module-level and local `from` imports (KIS001/KIS002), `# noqa`
/// comments both used and stale (KNQ), `print` / `breakpoint` lines (KPT),
/// and fixtures, calls and nesting for the structural rules (KST).
fn python_module(repeat: usize) -> String {
    let mut out = String::from(
        "\"\"\"Generated benchmark module.\"\"\"\n\
         from __future__ import annotations\n\n\
         import os\n\
         import json\n\
         from collections import abc\n\
         from os.path import join, dirname\n\
         from json import loads  # noqa: KIS001\n\
         from typing import Any, Optional\n\n\
         import pytest\n\
         import requests\n\n",
    );
    for i in 0..repeat {
        write!(
            out,
            r#"

class Service{i}:
    """A service with some state."""

    retries: int = {i}

    def __init__(self, root: str, options: Optional[dict[str, Any]] = None) -> None:
        self.root = root
        self.options = options or {{}}
        self.cache: dict[str, Any] = {{}}

    def load(self, name: str) -> Any:
        from os.path import exists
        path = join(self.root, name)
        if not exists(path):
            print("missing", path)
            return None
        with open(path, encoding="utf-8") as fh:
            data = loads(fh.read())
        self.cache[name] = data
        return data

    async def fetch(self, url: str) -> int:
        response = requests.get(url, timeout=self.retries)
        for attempt in range(self.retries):
            try:
                return response.status_code + attempt
            except ValueError as exc:  # noqa: E722
                raise RuntimeError(url) from exc
        return 0

    def run(self, command: str) -> int:
        result = os.system(command)  # noqa: KST003
        print(f"ran {{command}}: {{result}}")
        return result


def helper_{i}(values: abc.Iterable[int]) -> list[int]:
    total = [v * {i} for v in values if v % 2]
    lookup = {{k: str(v) for k, v in enumerate(total)}}
    print(json.dumps(lookup))  # noqa: KPT001
    return sorted(total, key=lambda v: (v, dirname(str(v))))


@pytest.fixture
def service_{i}(tmp_path: Any) -> Service{i}:
    service = Service{i}(str(tmp_path))
    assert service.retries == {i}
    return service


def test_service_{i}(service_{i}: Service{i}) -> None:
    assert service_{i}.load("missing.json") is None
    if service_{i}.retries > 100:
        breakpoint()
"#
        )
        .unwrap();
    }
    out
}
