use super::*;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// `alib.models` publishes `suba` and `subb`; `subc` exists but is neither
/// in `__all__` nor imported; `subd` is in `__all__` but never imported.
const MODELS_INIT: &str = "from . import suba, subb\nfrom .helpers import Thing\n\
                           __all__ = ['suba', 'subb', 'subd', 'Thing']\n";

struct Project {
    dir: TempDir,
    rule: Kis003Rule,
}

impl Project {
    fn new(extra: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().unwrap();
        let mut files = vec![
            (
                "alib/__init__.py",
                "from . import models\n__all__ = ['models']\n",
            ),
            ("alib/models/__init__.py", MODELS_INIT),
            ("alib/models/suba.py", "def run(): ...\n"),
            ("alib/models/subb.py", "def run(): ...\n"),
            ("alib/models/subc.py", ""),
            ("alib/models/subd.py", ""),
            ("alib/models/helpers.py", "class Thing: ...\n"),
            ("other/__init__.py", ""),
            ("other/models.py", ""),
        ];
        files.extend_from_slice(extra);
        for (path, body) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        let probe = ModuleProbe::for_roots(&[dir.path().to_path_buf()]);
        Self {
            dir,
            rule: Kis003Rule::new(Arc::new(probe)),
        }
    }

    fn ctx_at(&self, rel: &str, source: &str) -> FileContext {
        let path: PathBuf = self.dir.path().join(rel);
        FileContext::from_source(path, source.to_owned())
    }

    fn check_with(&self, source: &str, cfg: &toml::Value) -> Vec<Violation> {
        self.rule.check(&self.ctx_at("app.py", source), cfg)
    }

    fn check(&self, source: &str) -> Vec<Violation> {
        self.check_with(source, &empty_cfg())
    }

    fn fix(&self, source: &str) -> Option<String> {
        self.rule
            .fix(&self.ctx_at("app.py", source), &empty_cfg())
            .unwrap()
    }
}

fn empty_cfg() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

fn cfg(text: &str) -> toml::Value {
    toml::from_str(text).unwrap()
}

// ── detection ────────────────────────────────────────────────────────────

#[test]
fn reexported_submodules_are_flagged() {
    let p = Project::new(&[]);
    let v = p.check("from alib.models import suba\nfrom alib.models import subb\n");
    assert_eq!(v.len(), 2, "{v:?}");
    assert_eq!(v[0].rule, "KIS003");
    assert_eq!(v[0].line, 1);
    assert_eq!(v[1].line, 2);
    assert_eq!(v[0].level, Level::Warning);
    assert!(v[0].fixable);
    assert!(v[0].message.contains("'alib.models'") && v[0].message.contains("'suba'"));
    let help = v[0].help.as_deref().unwrap();
    assert!(help.contains("`from alib import models`"), "{help}");
    assert!(help.contains("`models.suba`"), "{help}");
}

#[test]
fn not_flagged_cases() {
    let p = Project::new(&[]);
    for source in [
        // in neither __all__ nor imported by the package
        "from alib.models import subc\n",
        // in __all__ but never imported: `models.subd` would fail at runtime
        "from alib.models import subd\n",
        // an object, not a submodule: KIS001's business
        "from alib.models import Thing\n",
        // top-level package: never suggest a bare `import alib`
        "from alib import models\n",
        // package without __init__ exports at all
        "from other import models\n",
        // star and relative imports
        "from alib.models import *\n",
        "from .models import suba\n",
        // re-exported by this file on purpose
        "from alib.models import suba\n__all__ = ['suba']\n",
        // not installed
        "from missing.pkg import thing\n",
        // suppressed
        "from alib.models import suba  # noqa: KIS003\n",
        "from alib.models import suba  # noqa: KIS\n",
    ] {
        assert!(
            p.check(source).is_empty(),
            "unexpected violation for {source:?}"
        );
    }
}

#[test]
fn noqa_on_one_line_of_a_paren_import_suppresses_only_that_alias() {
    let p = Project::new(&[]);
    let v = p.check("from alib.models import (\n    suba,  # noqa: KIS003\n    subb,\n)\n");
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].message.contains("'subb'"));
}

#[test]
fn package_without_all_is_not_flagged() {
    let p = Project::new(&[
        ("blib/__init__.py", ""),
        ("blib/core/__init__.py", "from . import sub\n"),
        ("blib/core/sub.py", ""),
    ]);
    assert!(p.check("from blib.core import sub\n").is_empty());
}

#[test]
fn files_inside_the_package_are_not_flagged() {
    let p = Project::new(&[]);
    let source = "from alib.models import suba\n";
    for rel in ["alib/models/subc.py", "alib/models/__init__.py"] {
        let v = p.rule.check(&p.ctx_at(rel, source), &empty_cfg());
        assert!(v.is_empty(), "{rel}: {v:?}");
    }
    // A sibling package of `alib.models` is outside it.
    let v = p
        .rule
        .check(&p.ctx_at("alib/views.py", source), &empty_cfg());
    assert_eq!(v.len(), 1);
}

#[test]
fn exceptions_and_level_options() {
    let p = Project::new(&[]);
    let source = "from alib.models import suba\n";
    assert!(p
        .check_with(source, &cfg("exceptions = [\"alib.models\"]"))
        .is_empty());
    let v = p.check_with(source, &cfg("level = \"error\""));
    assert_eq!(v[0].level, Level::Error);
}

#[test]
fn shadowed_package_name_is_reported_but_not_fixable() {
    let p = Project::new(&[]);
    let source = "from alib.models import suba\n\ndef f(models):\n    return suba.run()\n";
    let v = p.check(source);
    assert_eq!(v.len(), 1);
    assert!(!v[0].fixable);
    assert!(v[0]
        .help
        .as_deref()
        .unwrap()
        .contains("'models' is also assigned"));
    assert_eq!(p.fix(source), None);
}

#[test]
fn shadowed_old_name_is_reported_but_not_fixable() {
    let p = Project::new(&[]);
    let source =
        "from alib.models import suba\n\ndef f():\n    suba = 1\n    return suba\n\nsuba.run()\n";
    let v = p.check(source);
    assert_eq!(v.len(), 1);
    assert!(!v[0].fixable);
    assert_eq!(p.fix(source), None);
}

#[test]
fn package_name_imported_from_elsewhere_is_not_fixable() {
    let p = Project::new(&[]);
    let source = "from other import models\nfrom alib.models import suba\n\nsuba.run()\nmodels\n";
    let v = p.check(source);
    assert_eq!(v.len(), 1);
    assert!(!v[0].fixable);
    assert!(v[0].help.as_deref().unwrap().contains("already imported"));
    assert_eq!(p.fix(source), None);
}

// ── fixes ────────────────────────────────────────────────────────────────

#[test]
fn fix_merges_into_one_package_import_and_qualifies_uses() {
    let p = Project::new(&[]);
    let fixed = p
        .fix("from alib.models import suba\nfrom alib.models import subb\n\nsuba.run()\nsubb.run()\n")
        .unwrap();
    assert_eq!(
        fixed,
        "from alib import models\n\nmodels.suba.run()\nmodels.subb.run()\n"
    );
}

#[test]
fn fix_handles_aliases() {
    let p = Project::new(&[]);
    let fixed = p
        .fix("from alib.models import suba as sa\n\nsa.run()\n")
        .unwrap();
    assert_eq!(fixed, "from alib import models\n\nmodels.suba.run()\n");
}

#[test]
fn fix_keeps_unflagged_names_of_the_same_statement() {
    let p = Project::new(&[]);
    let fixed = p
        .fix("from alib.models import (\n    subc,\n    suba,\n)\n\nsuba.run()\nsubc\n")
        .unwrap();
    assert_eq!(
        fixed,
        "from alib.models import subc\nfrom alib import models\n\nmodels.suba.run()\nsubc\n"
    );
}

#[test]
fn fix_reuses_an_existing_package_import() {
    let p = Project::new(&[]);
    let fixed = p
        .fix("from alib import models\nfrom alib.models import suba\n\nsuba.run()\nmodels\n")
        .unwrap();
    assert_eq!(
        fixed,
        "from alib import models\n\nmodels.suba.run()\nmodels\n"
    );
}

#[test]
fn fix_inside_a_function_stays_in_the_function() {
    let p = Project::new(&[]);
    let fixed = p
        .fix("def f():\n    from alib.models import suba\n    return suba.run()\n")
        .unwrap();
    assert_eq!(
        fixed,
        "def f():\n    from alib import models\n    return models.suba.run()\n"
    );
}

#[test]
fn fix_inside_type_checking_stays_in_the_block() {
    let p = Project::new(&[]);
    let fixed = p
        .fix("from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from alib.models import suba\n\nx: 'suba.T'\n")
        .unwrap();
    assert!(
        fixed.contains("if TYPE_CHECKING:\n    from alib import models\n"),
        "{fixed}"
    );
}

#[test]
fn fix_leaves_strings_comments_and_attributes_alone() {
    let p = Project::new(&[]);
    let fixed = p
        .fix("from alib.models import suba\n\n# suba here\nx = 'suba'\nobj.suba\nsuba.run()\n")
        .unwrap();
    assert_eq!(
        fixed,
        "from alib import models\n\n# suba here\nx = 'suba'\nobj.suba\nmodels.suba.run()\n"
    );
}

#[test]
fn fix_is_idempotent() {
    let p = Project::new(&[]);
    let once = p
        .fix("from alib.models import suba\n\nsuba.run()\n")
        .unwrap();
    assert_eq!(p.fix(&once), None);
    assert!(p.check(&once).is_empty());
}
