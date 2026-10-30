//! Benchmarks for the konform linting engine.
//!
//! konform is a binary-only crate, so the engine's modules are compiled into
//! this benchmark directly (the same module tree as `src/main.rs`, minus the
//! CLI, LSP and output layers it does not need).
#![allow(dead_code, unused_imports)]

#[path = "../src/config.rs"]
mod config;
#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/migrations/mod.rs"]
mod migrations;
#[path = "../src/module_probe.rs"]
mod module_probe;
#[path = "../src/rules/mod.rs"]
mod rules;
#[path = "../src/types.rs"]
mod types;

use config::Config;
use divan::{black_box, Bencher};
use engine::CheckInput;
use module_probe::ModuleProbe;
use rules::{all_rules, FileContext, Rule};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

fn main() {
    divan::main();
}

/// Number of repeated "sections" in the generated Python modules.
const SIZES: &[usize] = &[10, 100];

/// Project config enabling pattern (KPT) and structural (KST) rules on top of
/// the default rule set.
const KONFORM_TOML: &str = r#"
[konform.lint]
level = "error"

[[konform.lint.user-defined-patterns.rules]]
id      = "KPT001"
message = "Use the project logger instead of bare print()."
pattern = '^\s*print\s*\('
level   = "warning"

[[konform.lint.user-defined-patterns.rules]]
id      = "KPT002"
message = "Remove breakpoint()."
pattern = '^\s*breakpoint\s*\(\s*\)'

[[konform.lint.user-defined-patterns.rules]]
id      = "KPT003"
message = "TODO without a ticket reference."
pattern = '#\s*TODO[^(]'

[[konform.lint.user-defined-patterns.rules]]
id          = "KPT004"
message     = "Use logging.warning instead of logging.warn."
pattern     = 'logging\.warn\('
replacement = "logging.warning("

[[konform.lint.structural-rules.rules]]
id      = "KST001"
message = "Do not use assert inside a pytest fixture."
match   = { kind = "assert", inside = { decorated_with = "pytest.fixture" } }

[[konform.lint.structural-rules.rules]]
id      = "KST002"
message = "Use the project logger instead of print()."
match   = { callee = "print" }
fix     = { replace = "logger.info({args})", safe = true }
"#;

/// One section of realistic Python code exercising every rule family:
/// symbol imports (KIS001), needless aliases (KIS002), `# noqa` comments
/// (KNQ*), regex patterns (KPT) and structural matches (KST).
fn python_section(i: usize) -> String {
    format!(
        r#"
from os.path import join as join_{i}
from collections import OrderedDict
import os.path as path_{i}
import json
import logging
import pytest


class Service{i}:
    """A small service class number {i}."""

    def __init__(self, root: str) -> None:
        self.root = root
        self.items = OrderedDict()

    def load(self, name: str) -> dict:
        full = join_{i}(self.root, name)
        if not path_{i}.exists(full):
            print("missing", full)
            return {{}}
        with open(full) as fh:
            data = json.load(fh)
        for key, value in data.items():
            self.items[key] = value
        return data

    def dump(self) -> str:
        logging.warn("dumping %d items", len(self.items))  # TODO clean up
        return json.dumps(self.items, sort_keys=True)  # noqa: KPT001


@pytest.fixture
def service_{i}(tmp_path):
    svc = Service{i}(str(tmp_path))
    assert svc.root
    return svc


def helper_{i}(values: list[int]) -> int:
    total = 0
    for v in values:
        if v % 2 == 0:
            total += v  # noqa
        else:
            total -= v  # noqa: KIS001
    lambda_ = lambda x: x * {i}
    return lambda_(total)
"#
    )
}

fn python_source(sections: usize) -> String {
    let mut src = String::from("\"\"\"Generated benchmark module.\"\"\"\n");
    for i in 0..sections {
        src.push_str(&python_section(i));
    }
    src
}

/// Shared module probe: querying the Python interpreter happens once, outside
/// of any measured region.
fn probe() -> Arc<ModuleProbe> {
    static PROBE: OnceLock<Arc<ModuleProbe>> = OnceLock::new();
    Arc::clone(PROBE.get_or_init(|| Arc::new(ModuleProbe::default())))
}

/// Config loaded from a temporary `konform.toml` with KPT and KST rules.
fn project_config() -> &'static (Config, PathBuf) {
    static CONFIG: OnceLock<(Config, PathBuf)> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let file = dir.join("konform.toml");
        std::fs::write(&file, KONFORM_TOML).expect("write konform.toml");
        let config = config::load_config(None, Some(&file));
        assert!(
            config.errors.is_empty(),
            "config errors: {:?}",
            config.errors
        );
        (config, dir)
    })
}

fn default_rules() -> Vec<Box<dyn Rule>> {
    all_rules(probe(), None)
}

fn project_rules() -> Vec<Box<dyn Rule>> {
    all_rules(probe(), Some(project_config().1.clone()))
}

/// Warm per-rule caches (compiled patterns, module probe results) so the
/// measured iterations reflect steady-state linting.
fn warm(rules: &[Box<dyn Rule>], config: &Config, path: &Path, source: &str) {
    let _ = engine::run_check(&CheckInput::new(path, source), rules, config);
}

/// Parsing a file and building its `FileContext` (AST, line index).
#[divan::bench(args = SIZES)]
fn parse_file_context(bencher: Bencher, sections: usize) {
    let source = python_source(sections);
    let path = PathBuf::from("pkg/module.py");
    bencher.bench(|| {
        let ctx = FileContext::from_source(black_box(path.clone()), black_box(source.clone()));
        black_box(ctx.stmts().len())
    });
}

/// `engine::run_check` with the default configuration and every built-in rule.
#[divan::bench(args = SIZES)]
fn check_default_config(bencher: Bencher, sections: usize) {
    let source = python_source(sections);
    let path = PathBuf::from("pkg/module.py");
    let rules = default_rules();
    let config = Config::default();
    warm(&rules, &config, &path, &source);
    bencher.bench(|| {
        let input = CheckInput::new(black_box(&path), black_box(&source));
        black_box(engine::run_check(&input, &rules, &config))
    });
}

/// `engine::run_check` with user-defined pattern (KPT) and structural (KST)
/// rules enabled through a project config.
#[divan::bench(args = SIZES)]
fn check_project_config(bencher: Bencher, sections: usize) {
    let source = python_source(sections);
    let (config, dir) = project_config();
    let path = dir.join("pkg/module.py");
    let rules = project_rules();
    warm(&rules, config, &path, &source);
    bencher.bench(|| {
        let input = CheckInput::new(black_box(&path), black_box(&source));
        black_box(engine::run_check(&input, &rules, config))
    });
}

/// `engine::run_check` on a clean file (no violations), the common case in CI.
#[divan::bench(args = SIZES)]
fn check_clean_file(bencher: Bencher, sections: usize) {
    let mut source = String::from("\"\"\"Clean module.\"\"\"\nimport json\nimport os.path\n");
    for i in 0..sections {
        source.push_str(&format!(
            "\n\ndef func_{i}(values: list[int]) -> str:\n    \"\"\"Doc.\"\"\"\n    total = sum(v * {i} for v in values)\n    return json.dumps({{\"path\": os.path.join(\"a\", str(total))}})\n"
        ));
    }
    let path = PathBuf::from("pkg/clean.py");
    let rules = default_rules();
    let config = Config::default();
    warm(&rules, &config, &path, &source);
    bencher.bench(|| {
        let input = CheckInput::new(black_box(&path), black_box(&source));
        black_box(engine::run_check(&input, &rules, &config))
    });
}

/// `engine::run_fix`: iteratively apply every safe fix until the file is stable.
#[divan::bench(args = SIZES)]
fn fix_project_config(bencher: Bencher, sections: usize) {
    let source = python_source(sections);
    let (config, dir) = project_config();
    let path = dir.join("pkg/module.py");
    let rules = project_rules();
    warm(&rules, config, &path, &source);
    bencher.bench(|| {
        let input = CheckInput::new(black_box(&path), black_box(&source));
        black_box(engine::run_fix(&input, &rules, config, false).expect("fix"))
    });
}
